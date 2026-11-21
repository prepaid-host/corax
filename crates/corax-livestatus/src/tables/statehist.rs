//! The `statehist` table: the state history of hosts and services, rebuilt from
//! `nagios.log` and the archive. The Checkmk GUI computes availability from it.
//!
//! A query runs in three steps so that the world lock is never held during file I/O:
//!
//! 1. [`Objects::collect`] (world lock): the hosts and services of the current
//!    configuration, and which services the query can possibly ask for.
//! 2. [`compute`] (no lock): replay the log files of the query period through a state
//!    machine per object. Every change of an object yields one [`Span`].
//! 3. The spans are the table rows; filters, columns and stats are evaluated as usual.
//!
//! The state machine follows Livestatus line by line, including its handling of core
//! restarts and log rotations, so the same log files give the same rows.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use corax_core::Paths;

use super::log::{parse_host_state, parse_service_state};
use super::{getter, Ctx, Getter, Row};
use crate::filter::Filter;
use crate::value::Value;

/// One row: an interval during which nothing changed for a host or service.
#[derive(Debug, Clone)]
pub struct Span {
    pub time: i64,
    pub lineno: i64,
    pub from: i64,
    pub until: i64,
    pub duration: i64,
    pub duration_part: f64,
    /// 0-3, or -1 for "unmonitored".
    pub state: i64,
    pub host_down: bool,
    pub in_downtime: bool,
    pub in_host_downtime: bool,
    pub is_flapping: bool,
    pub in_notification_period: i64,
    pub in_service_period: i64,
    pub notification_period: Arc<str>,
    pub service_period: Arc<str>,
    pub debug_info: &'static str,
    pub host_name: Arc<str>,
    /// Empty for hosts.
    pub service_description: Arc<str>,
    pub log_output: Arc<str>,
    pub long_log_output: Arc<str>,
}

impl Span {
    /// A row that only carries the object, for evaluating object filters.
    fn of_object(obj: &Object) -> Span {
        Span {
            time: 0,
            lineno: 0,
            from: 0,
            until: 0,
            duration: 0,
            duration_part: 0.0,
            state: 0,
            host_down: false,
            in_downtime: false,
            in_host_downtime: false,
            is_flapping: false,
            in_notification_period: 0,
            in_service_period: 0,
            notification_period: obj.notification_period.clone(),
            service_period: obj.service_period.clone(),
            debug_info: "",
            host_name: obj.host_name.clone(),
            service_description: obj.service_description.clone(),
            log_output: empty(),
            long_log_output: empty(),
        }
    }

    fn duration_in(&self, state: i64) -> i64 {
        if self.state == state {
            self.duration
        } else {
            0
        }
    }

    fn part_in(&self, state: i64) -> f64 {
        if self.state == state {
            self.duration_part
        } else {
            0.0
        }
    }
}

fn empty() -> Arc<str> {
    static EMPTY: OnceLock<Arc<str>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::from("")).clone()
}

struct Object {
    host_name: Arc<str>,
    service_description: Arc<str>,
    /// Index of the host object (the object itself for hosts).
    host: usize,
    notification_period: Arc<str>,
    service_period: Arc<str>,
    /// `false` if the query's filters rule this service out.
    wanted: bool,
}

struct HostEntry {
    index: usize,
    services: HashMap<String, usize>,
}

/// The hosts and services of the running configuration, by name.
#[derive(Default)]
pub struct Objects {
    list: Vec<Object>,
    hosts: HashMap<String, HostEntry>,
}

/// Columns that only depend on the object, not on its history.
fn is_object_column(canonical: &str) -> bool {
    canonical.starts_with("current_host_")
        || canonical.starts_with("current_service_")
        || canonical == "host_name"
        || canonical == "service_description"
}

impl Objects {
    /// Snapshot the configured objects. Services that `filter` can never accept are
    /// marked so that their log lines are skipped; hosts are always kept because
    /// their state is part of the history of their services.
    pub fn collect(ctx: &Ctx<'_>, filter: &Filter) -> Objects {
        let w = ctx.world;
        let object_filter = filter.partial(&is_object_column);
        let mut objects = Objects::default();
        for h in &w.hosts {
            let c = &h.cfg.common;
            let host = objects.add_host(
                &h.cfg.name,
                c.notification_period.as_deref().unwrap_or_default(),
                c.custom_var("SERVICE_PERIOD").unwrap_or_default(),
            );
            for sid in &h.services {
                let s = w.service(*sid);
                let c = &s.cfg.common;
                let index = objects.add_service(
                    host,
                    &s.cfg.description,
                    c.notification_period.as_deref().unwrap_or_default(),
                    c.custom_var("SERVICE_PERIOD").unwrap_or_default(),
                );
                if let Some(f) = &object_filter {
                    let row = Span::of_object(&objects.list[index]);
                    objects.list[index].wanted = f.accepts(ctx, &Row::StateHist(&row));
                }
            }
        }
        objects
    }

    fn add_host(&mut self, name: &str, notification_period: &str, service_period: &str) -> usize {
        let index = self.list.len();
        self.list.push(Object {
            host_name: Arc::from(name),
            service_description: empty(),
            host: index,
            notification_period: Arc::from(notification_period),
            service_period: Arc::from(service_period),
            wanted: true,
        });
        self.hosts.insert(name.to_string(), HostEntry { index, services: HashMap::new() });
        index
    }

    fn add_service(
        &mut self,
        host: usize,
        description: &str,
        notification_period: &str,
        service_period: &str,
    ) -> usize {
        let index = self.list.len();
        let host_name = self.list[host].host_name.clone();
        if let Some(entry) = self.hosts.get_mut(&*host_name) {
            entry.services.insert(description.to_string(), index);
        }
        self.list.push(Object {
            host_name,
            service_description: Arc::from(description),
            host,
            notification_period: Arc::from(notification_period),
            service_period: Arc::from(service_period),
            wanted: true,
        });
        index
    }

    fn find(&self, host: &str, service: &str) -> Option<usize> {
        let h = self.hosts.get(host)?;
        if service.is_empty() {
            Some(h.index)
        } else {
            h.services.get(service).copied()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    LogVersion,
    LoggingInitialStates,
    CoreStarting,
    /// Core stopping and acknowledgements: no state change, but they end the block of
    /// initial states.
    Other,
    InitialHostState,
    CurrentHostState,
    HostAlert,
    HostDowntimeAlert,
    HostFlappingAlert,
    InitialServiceState,
    CurrentServiceState,
    ServiceAlert,
    ServiceDowntimeAlert,
    ServiceFlappingAlert,
    TimeperiodTransition,
}

/// Classify the text of a log line (after the timestamp); `None` for lines that do
/// not matter here. Kinds found in the first `match` have arguments after `TYPE: `.
fn classify(text: &str) -> Option<Kind> {
    if let Some(p) = text.find(':') {
        if text[p + 1..].starts_with(' ') {
            let kind = match &text[..p] {
                "INITIAL HOST STATE" => Some(Kind::InitialHostState),
                "CURRENT HOST STATE" => Some(Kind::CurrentHostState),
                "HOST ALERT" => Some(Kind::HostAlert),
                "HOST DOWNTIME ALERT" => Some(Kind::HostDowntimeAlert),
                "HOST FLAPPING ALERT" => Some(Kind::HostFlappingAlert),
                "INITIAL SERVICE STATE" => Some(Kind::InitialServiceState),
                "CURRENT SERVICE STATE" => Some(Kind::CurrentServiceState),
                "SERVICE ALERT" => Some(Kind::ServiceAlert),
                "SERVICE DOWNTIME ALERT" => Some(Kind::ServiceDowntimeAlert),
                "SERVICE FLAPPING ALERT" => Some(Kind::ServiceFlappingAlert),
                "TIMEPERIOD TRANSITION" => Some(Kind::TimeperiodTransition),
                "HOST ACKNOWLEDGE ALERT" | "SERVICE ACKNOWLEDGE ALERT" => Some(Kind::Other),
                // typed lines of other log classes
                "HOST NOTIFICATION"
                | "SERVICE NOTIFICATION"
                | "HOST NOTIFICATION RESULT"
                | "SERVICE NOTIFICATION RESULT"
                | "HOST NOTIFICATION PROGRESS"
                | "SERVICE NOTIFICATION PROGRESS"
                | "HOST ALERT HANDLER STARTED"
                | "SERVICE ALERT HANDLER STARTED"
                | "HOST ALERT HANDLER STOPPED"
                | "SERVICE ALERT HANDLER STOPPED"
                | "PASSIVE SERVICE CHECK"
                | "PASSIVE HOST CHECK"
                | "EXTERNAL COMMAND" => return None,
                _ => None,
            };
            if kind.is_some() {
                return kind;
            }
        }
    }
    if text.starts_with("LOG VERSION: 2.0") {
        Some(Kind::LogVersion)
    } else if text.starts_with("logging initial states") || text.starts_with("logging intitial states") {
        Some(Kind::LoggingInitialStates)
    } else if text.contains("starting...") || text.contains("active mode...") {
        Some(Kind::CoreStarting)
    } else if text.contains("shutting down...") || text.contains("Bailing out") || text.contains("standby mode...") {
        Some(Kind::Other)
    } else {
        None
    }
}

/// The fields of a state, downtime or flapping line.
#[derive(Default)]
struct Fields<'a> {
    host_name: &'a str,
    service_description: &'a str,
    state: i64,
    state_type: &'a str,
    plugin_output: &'a str,
    long_plugin_output: &'a str,
}

fn fields(kind: Kind, args: &str) -> Fields<'_> {
    let mut parts = args.split(';');
    let mut next = || parts.next().unwrap_or("");
    let mut f = Fields { host_name: next(), ..Default::default() };
    match kind {
        Kind::InitialHostState | Kind::CurrentHostState | Kind::HostAlert => {
            f.state = parse_host_state(next());
            f.state_type = next();
            next(); // attempt
            f.plugin_output = next();
            f.long_plugin_output = next();
        }
        Kind::InitialServiceState | Kind::CurrentServiceState | Kind::ServiceAlert => {
            f.service_description = next();
            f.state = parse_service_state(next());
            f.state_type = next();
            next(); // attempt
            f.plugin_output = next();
            f.long_plugin_output = next();
        }
        Kind::HostDowntimeAlert | Kind::HostFlappingAlert => f.state_type = next(),
        Kind::ServiceDowntimeAlert | Kind::ServiceFlappingAlert => {
            f.service_description = next();
            f.state_type = next();
        }
        _ => {}
    }
    f
}

/// A relevant line of a log file: `text[args..end]` are its arguments.
#[derive(Clone, Copy)]
struct Line {
    time: i64,
    lineno: i64,
    kind: Kind,
    args: usize,
    end: usize,
}

/// A log file read into memory, reduced to the lines that matter.
struct LogFile {
    data: Vec<u8>,
    lines: Vec<Line>,
}

impl LogFile {
    fn read(path: &Path) -> LogFile {
        let data = std::fs::read(path).unwrap_or_default();
        let mut lines = Vec::new();
        let mut sorted = true;
        let mut last = i64::MIN;
        let mut start = 0;
        let mut lineno = 0;
        while start < data.len() {
            let end = data[start..].iter().position(|&b| b == b'\n').map_or(data.len(), |p| start + p);
            lineno += 1;
            let raw = &data[start..end];
            if raw.len() >= 13 && raw[0] == b'[' && raw[11] == b']' && raw[12] == b' ' {
                let time = std::str::from_utf8(&raw[1..11]).ok().and_then(|t| t.parse::<i64>().ok());
                if let Some(time) = time {
                    if let Some(kind) = classify(&String::from_utf8_lossy(&raw[13..])) {
                        // the arguments follow the first ": " (offsets in the raw bytes)
                        let colon = raw.iter().position(|&b| b == b':').unwrap_or(raw.len());
                        let args = (start + colon + 2).min(end);
                        sorted &= time >= last;
                        last = time;
                        lines.push(Line { time, lineno, kind, args, end });
                    }
                }
            }
            start = end + 1;
        }
        if !sorted {
            // Livestatus keeps the entries of a file ordered by (time, line number)
            lines.sort_by_key(|l| (l.time, l.lineno));
        }
        LogFile { data, lines }
    }

    fn args(&self, line: &Line) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.data[line.args..line.end])
    }
}

/// First timestamp of every log file, oldest first.
fn log_files(paths: &Paths) -> Vec<(i64, PathBuf)> {
    type Known = HashMap<PathBuf, ((u64, u64), i64)>;
    static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();

    let mut candidates: Vec<PathBuf> = Vec::new();
    // the current log file wins over an archive file with the same start time
    candidates.extend(paths.log_file.iter().cloned());
    if let Some(dir) = &paths.log_archive_path {
        if let Ok(rd) = std::fs::read_dir(dir) {
            candidates.extend(rd.filter_map(Result::ok).map(|e| e.path()));
        }
    }
    let mut known = KNOWN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let mut files: BTreeMap<i64, PathBuf> = BTreeMap::new();
    let mut seen: HashMap<PathBuf, ((u64, u64), i64)> = HashMap::with_capacity(candidates.len());
    for path in candidates {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        let id = (meta.dev(), meta.ino());
        let since = match known.get(&path) {
            Some((known_id, since)) if *known_id == id => Some(*since),
            _ => super::log::first_timestamp(&path),
        };
        let Some(since) = since else { continue };
        seen.insert(path.clone(), (id, since));
        files.entry(since).or_insert(path);
    }
    *known = seen;
    files.into_iter().collect()
}

/// The history of one host or service while the log is replayed.
struct State {
    object: usize,
    is_host: bool,
    time: i64,
    lineno: i64,
    from: i64,
    until: i64,
    host_down: bool,
    state: i64,
    in_notification_period: i64,
    in_service_period: i64,
    downtime_depth: i64,
    host_downtime_depth: i64,
    is_flapping: bool,
    /// For hosts: the states of its services.
    services: Vec<usize>,
    may_no_longer_exist: bool,
    has_vanished: bool,
    last_known_time: i64,
    debug_info: &'static str,
    log_output: Arc<str>,
    long_log_output: Arc<str>,
}

struct Machine<'a> {
    objects: &'a Objects,
    since: i64,
    until: i64,
    /// Indexed like `objects.list`.
    states: Vec<Option<State>>,
    /// Objects with a state, in order of appearance.
    order: Vec<usize>,
    /// Timeperiod name -> active (1) / inactive (0).
    time_periods: HashMap<String, i64>,
    only_update: bool,
    spans: Vec<Span>,
}

const TEN_MINUTES: i64 = 600;

impl Machine<'_> {
    fn period_state(&self, name: &str) -> i64 {
        // no information: assume "within the period"
        self.time_periods.get(name).copied().unwrap_or(1)
    }

    fn state_mut(&mut self, index: usize) -> &mut State {
        self.states[index].as_mut().expect("state exists")
    }

    /// Emit the span `[from, until)` of a state and start the next one.
    fn process(&mut self, index: usize) {
        let timeframe = (self.until - self.since) as f64;
        let hss = self.states[index].as_mut().expect("state exists");
        let duration = hss.until - hss.from;
        if duration != 0 {
            let obj = &self.objects.list[hss.object];
            self.spans.push(Span {
                time: hss.time,
                lineno: hss.lineno,
                from: hss.from,
                until: hss.until,
                duration,
                duration_part: duration as f64 / timeframe,
                state: hss.state,
                host_down: hss.host_down,
                in_downtime: hss.downtime_depth > 0,
                in_host_downtime: hss.host_downtime_depth > 0,
                is_flapping: hss.is_flapping,
                in_notification_period: hss.in_notification_period,
                in_service_period: hss.in_service_period,
                notification_period: obj.notification_period.clone(),
                service_period: obj.service_period.clone(),
                debug_info: hss.debug_info,
                host_name: obj.host_name.clone(),
                service_description: obj.service_description.clone(),
                log_output: hss.log_output.clone(),
                long_log_output: hss.long_log_output.clone(),
            });
        }
        hss.from = hss.until;
    }

    fn process_unless_updating(&mut self, index: usize) {
        if !self.only_update {
            self.process(index);
        }
    }

    /// Objects that did not show up in the block of initial states are gone.
    fn set_unknown_to_unmonitored(&mut self) {
        for hss in self.states.iter_mut().flatten() {
            if hss.may_no_longer_exist {
                hss.has_vanished = true;
            }
        }
    }

    /// `logging initial states`: the state of every object that still exists follows.
    fn handle_log_initial_states(&mut self, time: i64) {
        for hss in self.states.iter_mut().flatten() {
            if !hss.has_vanished {
                hss.last_known_time = time;
                hss.may_no_longer_exist = true;
            }
        }
    }

    fn reset_downtime_depths(&mut self, time: i64) {
        for i in 0..self.order.len() {
            let index = self.order[i];
            let hss = self.state_mut(index);
            if hss.downtime_depth != 0 || hss.host_downtime_depth != 0 {
                hss.until = time;
                self.process_unless_updating(index);
                let hss = self.state_mut(index);
                hss.debug_info = "RESET DOWNTIME DEPTHS";
                hss.downtime_depth = 0;
                hss.host_downtime_depth = 0;
            }
        }
    }

    fn handle_state_entry(&mut self, line: &Line, f: &Fields<'_>) {
        let Some(index) = self.state_for_entry(line, f) else { return };
        let changed = self.update_state(line, f, index);
        // host downtimes and state changes also affect the services of the host
        if changed && matches!(line.kind, Kind::HostAlert | Kind::CurrentHostState | Kind::HostDowntimeAlert) {
            let services = std::mem::take(&mut self.state_mut(index).services);
            for &svc in &services {
                self.update_state(line, f, svc);
            }
            self.state_mut(index).services = services;
        }
    }

    fn state_for_entry(&mut self, line: &Line, f: &Fields<'_>) -> Option<usize> {
        // objects that are no longer configured have no history
        let index = self.objects.find(f.host_name, f.service_description)?;
        let obj = &self.objects.list[index];
        if !obj.wanted {
            return None;
        }
        if self.states[index].is_none() {
            let is_host = f.service_description.is_empty();
            let mut hss = State {
                object: index,
                is_host,
                time: 0,
                lineno: 0,
                from: self.since,
                until: 0,
                host_down: false,
                state: 0,
                in_notification_period: self.period_state(&obj.notification_period),
                in_service_period: self.period_state(&obj.service_period),
                downtime_depth: 0,
                host_downtime_depth: 0,
                is_flapping: false,
                services: Vec::new(),
                may_no_longer_exist: false,
                has_vanished: false,
                last_known_time: 0,
                debug_info: "",
                log_output: Arc::from("No information about that period of time available"),
                long_log_output: empty(),
            };
            if is_host {
                hss.services =
                    self.order.iter().copied().filter(|&i| i != index && self.objects.list[i].host == index).collect();
            } else if let Some(host) = self.states[obj.host].as_mut() {
                host.services.push(index);
                hss.host_downtime_depth = host.host_downtime_depth;
                hss.host_down = host.host_down;
            }
            // An object that appears within the query period was not monitored before
            // (with a grace period of ten minutes for the start of the core).
            if !self.only_update && line.time - hss.from > TEN_MINUTES {
                hss.debug_info = "UNMONITORED ";
                hss.state = -1;
            }
            self.states[index] = Some(hss);
            self.order.push(index);
        }
        Some(index)
    }

    /// Apply a log line to a state. Returns `false` if nothing changed for a host.
    fn update_state(&mut self, line: &Line, f: &Fields<'_>, index: usize) -> bool {
        let mut changed = true;

        // revive an object that was unmonitored
        if self.state_mut(index).has_vanished {
            let hss = self.state_mut(index);
            hss.time = hss.last_known_time;
            hss.until = hss.last_known_time;
            self.process_unless_updating(index);

            let obj = &self.objects.list[index];
            let in_notification_period = self.period_state(&obj.notification_period);
            let in_service_period = self.period_state(&obj.service_period);
            let hss = self.state_mut(index);
            hss.may_no_longer_exist = false;
            hss.has_vanished = false;
            hss.state = -1;
            hss.debug_info = "UNMONITORED";
            hss.downtime_depth = 0;
            hss.is_flapping = false;
            hss.log_output = empty();
            hss.long_log_output = empty();
            hss.in_notification_period = in_notification_period;
            hss.in_service_period = in_service_period;
        }

        let hss = self.state_mut(index);
        hss.time = line.time;
        hss.lineno = line.lineno;
        hss.until = line.time;
        hss.may_no_longer_exist = false;
        let is_host = hss.is_host;

        match line.kind {
            Kind::CurrentHostState | Kind::InitialHostState | Kind::HostAlert => {
                if is_host {
                    if hss.state != f.state {
                        self.process_unless_updating(index);
                        let hss = self.state_mut(index);
                        hss.state = f.state;
                        hss.host_down = f.state > 0;
                        hss.debug_info = "HOST STATE";
                    } else {
                        changed = false;
                    }
                } else if hss.host_down != (f.state > 0) {
                    self.process_unless_updating(index);
                    let hss = self.state_mut(index);
                    hss.host_down = f.state > 0;
                    hss.debug_info = "SVC HOST STATE";
                }
            }
            Kind::CurrentServiceState | Kind::InitialServiceState | Kind::ServiceAlert => {
                if hss.state != f.state {
                    self.process_unless_updating(index);
                    let hss = self.state_mut(index);
                    hss.debug_info = "SVC ALERT";
                    hss.state = f.state;
                }
            }
            Kind::HostDowntimeAlert => {
                let started = f.state_type.starts_with("STARTED");
                if (hss.host_downtime_depth > 0) != started {
                    self.process_unless_updating(index);
                    self.state_mut(index).debug_info = if is_host { "HOST DOWNTIME" } else { "SVC HOST DOWNTIME" };
                }
                let step = if started { 1 } else { -1 };
                let hss = self.state_mut(index);
                hss.host_downtime_depth += step;
                if is_host {
                    hss.downtime_depth += step;
                }
            }
            Kind::ServiceDowntimeAlert => {
                let started = f.state_type.starts_with("STARTED");
                if (hss.downtime_depth > 0) != started {
                    self.process_unless_updating(index);
                    self.state_mut(index).debug_info = "DOWNTIME SERVICE";
                }
                self.state_mut(index).downtime_depth += if started { 1 } else { -1 };
            }
            Kind::HostFlappingAlert | Kind::ServiceFlappingAlert => {
                let flapping = f.state_type.starts_with("STARTED");
                if hss.is_flapping != flapping {
                    self.process_unless_updating(index);
                    let hss = self.state_mut(index);
                    hss.debug_info = "FLAPPING ";
                    hss.is_flapping = flapping;
                } else {
                    changed = false;
                }
            }
            _ => {}
        }

        let initial = matches!(line.kind, Kind::InitialHostState | Kind::InitialServiceState);
        let output = if initial && f.plugin_output == "(null)" { "" } else { f.plugin_output };
        let hss = self.state_mut(index);
        if !output.is_empty() || hss.log_output.is_empty() {
            hss.log_output = Arc::from(output);
        }
        if !f.long_plugin_output.is_empty() || hss.long_log_output.is_empty() {
            hss.long_log_output = Arc::from(f.long_plugin_output.replace("\\n", "\n"));
        }
        changed
    }

    /// `TIMEPERIOD TRANSITION: <name>;<from>;<to>`
    fn handle_timeperiod_transition(&mut self, line: &Line, args: &str) {
        let parts: Vec<&str> = args.split(';').collect();
        let [name, from, to] = parts[..] else { return };
        let (Ok(_), Ok(to)) = (from.trim().parse::<i64>(), to.trim().parse::<i64>()) else { return };
        self.time_periods.insert(name.to_string(), to);
        for i in 0..self.order.len() {
            let index = self.order[i];
            let obj = &self.objects.list[index];
            let hss = self.states[index].as_mut().expect("state exists");
            hss.time = line.time;
            hss.lineno = line.lineno;
            hss.until = line.time;
            if name == &*obj.notification_period && to != hss.in_notification_period {
                self.process_unless_updating(index);
                let hss = self.state_mut(index);
                hss.debug_info = "TIMEPERIOD ";
                hss.in_notification_period = to;
            }
            let hss = self.state_mut(index);
            if name == &*obj.service_period && to != hss.in_service_period {
                self.process_unless_updating(index);
                let hss = self.state_mut(index);
                hss.debug_info = "TIMEPERIOD ";
                hss.in_service_period = to;
            }
        }
    }

    /// Close the last span of every object at the end of the query period.
    fn final_reports(&mut self) {
        for i in 0..self.order.len() {
            let index = self.order[i];
            if self.state_mut(index).may_no_longer_exist {
                // the last known state lasted until the core was restarted
                let hss = self.state_mut(index);
                hss.time = hss.last_known_time;
                hss.until = hss.last_known_time;
                self.process(index);
                let hss = self.state_mut(index);
                hss.state = -1;
                hss.debug_info = "UNMONITORED";
                hss.log_output = empty();
                hss.long_log_output = empty();
            }
            // rows at the very end of the half-open query period would be filtered out
            let time = self.until - 1;
            let hss = self.state_mut(index);
            hss.time = time;
            hss.until = time;
            self.process(index);
        }
    }

    fn replay(&mut self, files: &[(i64, PathBuf)]) {
        if files.is_empty() {
            return;
        }
        // start with the newest log file that begins before the query period
        let newest = files.len() - 1;
        let mut file_index = newest;
        while file_index > 0 && files[file_index].0 >= self.since {
            file_index -= 1;
        }
        if files[file_index].0 >= self.until {
            return;
        }
        let mut file = LogFile::read(&files[file_index].1);
        // The first relevant line of the first file is not evaluated, and a file whose
        // lines all predate the query period is skipped (like Livestatus).
        let skip_file =
            file_index != newest && file.lines.last().is_some_and(|l| file.lines.len() == 1 || l.time < self.since);
        let mut pos = if skip_file { file.lines.len() } else { 1 };

        let mut in_initial_states = false;
        let mut log_has_been_rotated = false;
        loop {
            while pos >= file.lines.len() {
                file_index += 1;
                if file_index > newest || files[file_index].0 >= self.until {
                    return self.final_reports();
                }
                file = LogFile::read(&files[file_index].1);
                pos = 0;
            }
            let line = file.lines[pos];
            pos += 1;
            if line.time >= self.until {
                break;
            }
            if self.only_update && line.time >= self.since {
                // reached the query period: from now on spans are reported
                for hss in self.states.iter_mut().flatten() {
                    hss.from = self.since;
                    hss.until = self.since;
                }
                self.only_update = false;
            }
            if in_initial_states
                && !matches!(line.kind, Kind::InitialHostState | Kind::InitialServiceState | Kind::LoggingInitialStates)
            {
                self.set_unknown_to_unmonitored();
            }
            match line.kind {
                Kind::LogVersion => {
                    log_has_been_rotated = true;
                    in_initial_states = false;
                }
                Kind::CoreStarting => {
                    log_has_been_rotated = false;
                    in_initial_states = false;
                }
                Kind::Other => in_initial_states = false,
                Kind::InitialHostState | Kind::InitialServiceState => {
                    let args = file.args(&line);
                    self.handle_state_entry(&line, &fields(line.kind, &args));
                }
                Kind::TimeperiodTransition => {
                    let args = file.args(&line);
                    self.handle_timeperiod_transition(&line, &args);
                    in_initial_states = false;
                }
                Kind::LoggingInitialStates => {
                    if in_initial_states {
                        self.set_unknown_to_unmonitored();
                    }
                    self.handle_log_initial_states(line.time);
                    // Downtimes that are still active are logged again after a rotation.
                    if log_has_been_rotated {
                        self.reset_downtime_depths(line.time);
                    }
                    in_initial_states = true;
                }
                _ => {
                    let args = file.args(&line);
                    self.handle_state_entry(&line, &fields(line.kind, &args));
                    in_initial_states = false;
                }
            }
        }
        self.final_reports();
    }
}

/// Replay the log files and return the spans within `[since, until)`.
pub fn compute(paths: &Paths, objects: &Objects, since: i64, until: i64) -> Vec<Span> {
    if since >= until {
        return Vec::new();
    }
    let mut machine = Machine {
        objects,
        since,
        until,
        states: (0..objects.list.len()).map(|_| None).collect(),
        order: Vec::new(),
        time_periods: HashMap::new(),
        only_update: true,
        spans: Vec::new(),
    };
    machine.replay(&log_files(paths));
    machine.spans
}

macro_rules! own {
    ($x:ident => $e:expr) => {
        Some(getter(move |_c, r| match r {
            Row::StateHist($x) => $e,
            _ => Value::Null,
        }))
    };
}

pub fn column(name: &str) -> Option<Getter> {
    match name {
        "time" => own!(s => Value::Int(s.time)),
        "lineno" => own!(s => Value::Int(s.lineno)),
        "from" => own!(s => Value::Int(s.from)),
        "until" => own!(s => Value::Int(s.until)),
        "duration" => own!(s => Value::Int(s.duration)),
        "duration_part" => own!(s => Value::Float(s.duration_part)),
        "state" => own!(s => Value::Int(s.state)),
        "host_down" => own!(s => Value::bool(s.host_down)),
        "in_downtime" => own!(s => Value::bool(s.in_downtime)),
        "in_host_downtime" => own!(s => Value::bool(s.in_host_downtime)),
        "is_flapping" => own!(s => Value::bool(s.is_flapping)),
        "in_notification_period" => own!(s => Value::Int(s.in_notification_period)),
        "notification_period" => own!(s => Value::str(&*s.notification_period)),
        "in_service_period" => own!(s => Value::Int(s.in_service_period)),
        "service_period" => own!(s => Value::str(&*s.service_period)),
        "debug_info" => own!(s => Value::str(s.debug_info)),
        "host_name" => own!(s => Value::str(&*s.host_name)),
        "service_description" => own!(s => Value::str(&*s.service_description)),
        "log_output" => own!(s => Value::str(&*s.log_output)),
        "long_log_output" => own!(s => Value::str(&*s.long_log_output)),
        "duration_ok" => own!(s => Value::Int(s.duration_in(0))),
        "duration_part_ok" => own!(s => Value::Float(s.part_in(0))),
        "duration_warning" => own!(s => Value::Int(s.duration_in(1))),
        "duration_part_warning" => own!(s => Value::Float(s.part_in(1))),
        "duration_critical" => own!(s => Value::Int(s.duration_in(2))),
        "duration_part_critical" => own!(s => Value::Float(s.part_in(2))),
        "duration_unknown" => own!(s => Value::Int(s.duration_in(3))),
        "duration_part_unknown" => own!(s => Value::Float(s.part_in(3))),
        "duration_unmonitored" => own!(s => Value::Int(s.duration_in(-1))),
        "duration_part_unmonitored" => own!(s => Value::Float(s.part_in(-1))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_700_000_000;

    /// Replace `+N` at the start of a line by the timestamp `[T+N]`.
    fn log(text: &str) -> String {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                let (offset, rest) = l.split_once(' ').unwrap();
                format!("[{}] {rest}\n", T + offset.parse::<i64>().unwrap())
            })
            .collect()
    }

    fn site(current: &str, archive: &[&str]) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        std::fs::create_dir(&archive_dir).unwrap();
        for (i, text) in archive.iter().enumerate() {
            std::fs::write(archive_dir.join(format!("nagios-{i}.log")), log(text)).unwrap();
        }
        let log_file = dir.path().join("nagios.log");
        std::fs::write(&log_file, log(current)).unwrap();
        let paths = Paths { log_file: Some(log_file), log_archive_path: Some(archive_dir), ..Default::default() };
        (dir, paths)
    }

    /// Host `h` (notification period `work`) with the given services.
    fn objects(services: &[&str]) -> Objects {
        let mut o = Objects::default();
        let h = o.add_host("h", "work", "");
        for s in services {
            o.add_service(h, s, "", "");
        }
        o
    }

    /// `from-until state [flags] debug_info` of all spans of an object, relative to `T`.
    fn spans(all: &[Span], service: &str) -> Vec<String> {
        all.iter()
            .filter(|s| &*s.host_name == "h" && &*s.service_description == service)
            .map(|s| {
                let mut out = format!("{}-{} {}", s.from - T, s.until - T, s.state);
                for (flag, name) in [
                    (s.host_down, "host_down"),
                    (s.in_downtime, "downtime"),
                    (s.in_host_downtime, "host_downtime"),
                    (s.is_flapping, "flapping"),
                    (s.in_notification_period == 0, "out_of_period"),
                ] {
                    if flag {
                        out.push(' ');
                        out.push_str(name);
                    }
                }
                format!("{out} [{}]", s.debug_info.trim())
            })
            .collect()
    }

    const START: &str = "0 corax 0.3.0 starting... (PID=1)
        0 LOG VERSION: 2.0
        0 logging initial states
        0 INITIAL HOST STATE: h;UP;HARD;1;up
        0 INITIAL SERVICE STATE: h;s;OK;HARD;1;fine";

    #[test]
    fn classifies_lines() {
        assert_eq!(classify("SERVICE ALERT: h;s;OK;HARD;1;starting..."), Some(Kind::ServiceAlert));
        assert_eq!(classify("SERVICE NOTIFICATION: c;h;s;OK;cmd;starting..."), None);
        assert_eq!(classify("HOST NOTIFICATION RESULT: c;h;OK;cmd;starting..."), None);
        assert_eq!(classify("EXTERNAL COMMAND: LOG;shutting down..."), None);
        assert_eq!(classify("Caught SIGHUP, restarting..."), Some(Kind::CoreStarting));
        assert_eq!(classify("Nagios 3.5.1 starting... (PID=1)"), Some(Kind::CoreStarting));
        assert_eq!(classify("Caught SIGTERM, shutting down..."), Some(Kind::Other));
        assert_eq!(classify("HOST ACKNOWLEDGE ALERT: h;STARTED;me;why"), Some(Kind::Other));
        assert_eq!(classify("LOG VERSION: 2.0"), Some(Kind::LogVersion));
        assert_eq!(classify("logging initial states"), Some(Kind::LoggingInitialStates));
        assert_eq!(classify("TIMEPERIOD TRANSITION: 24X7;-1;1"), Some(Kind::TimeperiodTransition));
        assert_eq!(classify("LOG ROTATION: DAILY"), None);
        assert_eq!(classify("Auto-save of retention data completed successfully."), None);
    }

    #[test]
    fn state_changes_split_the_period() {
        let text = format!(
            "{START}
            100 SERVICE ALERT: h;s;CRITICAL;HARD;1;broken
            300 SERVICE ALERT: h;s;OK;HARD;1;fine again;with\\ndetails"
        );
        let (_dir, paths) = site(&text, &[]);
        let all = compute(&paths, &objects(&["s"]), T, T + 1000);
        assert_eq!(spans(&all, ""), ["0-999 0 []"]);
        assert_eq!(spans(&all, "s"), ["0-100 0 []", "100-300 2 [SVC ALERT]", "300-999 0 [SVC ALERT]"]);
        let s: Vec<&Span> = all.iter().filter(|s| &*s.service_description == "s").collect();
        assert_eq!((&*s[0].log_output, &*s[1].log_output), ("fine", "broken"));
        assert_eq!(&*s[2].long_log_output, "with\ndetails");
        assert_eq!((s[1].duration, s[1].duration_part), (200, 0.2));
        assert_eq!((s[1].time, s[1].lineno), (T + 300, 7));
        // an empty period has no rows
        assert!(compute(&paths, &objects(&["s"]), T + 10, T + 10).is_empty());
    }

    #[test]
    fn host_state_downtime_and_flapping_reach_the_services() {
        let text = format!(
            "{START}
            100 HOST DOWNTIME ALERT: h;STARTED; Host has entered a period of scheduled downtime
            200 HOST ALERT: h;DOWN;HARD;1;dead
            300 SERVICE FLAPPING ALERT: h;s;STARTED; Service appears to have started flapping
            400 HOST ALERT: h;UP;HARD;1;back
            500 HOST DOWNTIME ALERT: h;STOPPED; Host has exited from a period of scheduled downtime
            600 SERVICE FLAPPING ALERT: h;s;STOPPED; Service appears to have stopped flapping"
        );
        let (_dir, paths) = site(&text, &[]);
        let all = compute(&paths, &objects(&["s"]), T, T + 1000);
        assert_eq!(
            spans(&all, ""),
            [
                "0-100 0 []",
                "100-200 0 downtime host_downtime [HOST DOWNTIME]",
                "200-400 1 host_down downtime host_downtime [HOST STATE]",
                "400-500 0 downtime host_downtime [HOST STATE]",
                "500-999 0 [HOST DOWNTIME]",
            ]
        );
        assert_eq!(
            spans(&all, "s"),
            [
                "0-100 0 []",
                "100-200 0 host_downtime [SVC HOST DOWNTIME]",
                "200-300 0 host_down host_downtime [SVC HOST STATE]",
                "300-400 0 host_down host_downtime flapping [FLAPPING]",
                "400-500 0 host_downtime flapping [SVC HOST STATE]",
                "500-600 0 flapping [SVC HOST DOWNTIME]",
                "600-999 0 [FLAPPING]",
            ]
        );
    }

    #[test]
    fn rotation_keeps_downtimes_and_reports_removed_objects() {
        let before = format!(
            "{START}
            0 INITIAL SERVICE STATE: h;gone;OK;HARD;1;fine
            100 SERVICE DOWNTIME ALERT: h;s;STARTED; Service has entered a period of scheduled downtime"
        );
        // what corax writes at a rotation: `gone` is no longer part of the block
        let after = "1000 LOG ROTATION: DAILY
            1000 LOG VERSION: 2.0
            1000 logging initial states
            1000 CURRENT HOST STATE: h;UP;HARD;1;up
            1000 CURRENT SERVICE STATE: h;s;OK;HARD;1;fine
            1000 SERVICE DOWNTIME ALERT: h;s;STARTED; Service has entered a period of scheduled downtime
            1500 SERVICE DOWNTIME ALERT: h;s;STOPPED; Service has exited from a period of scheduled downtime";
        let (_dir, paths) = site(after, &[&before]);
        // the period starts within the first file
        let all = compute(&paths, &objects(&["s", "gone"]), T + 50, T + 2000);
        assert_eq!(spans(&all, ""), ["50-1000 0 []", "1000-1999 0 [HOST STATE]"]);
        assert_eq!(
            spans(&all, "s"),
            [
                "50-100 0 []",
                "100-1000 0 downtime [DOWNTIME SERVICE]",
                "1000-1500 0 downtime [DOWNTIME SERVICE]",
                "1500-1999 0 [DOWNTIME SERVICE]",
            ]
        );
        assert_eq!(spans(&all, "gone"), ["50-1000 0 []", "1000-1999 -1 [UNMONITORED]"]);
    }

    #[test]
    fn a_file_that_ends_before_the_period_is_not_read() {
        let old = format!("{START}\n100 SERVICE ALERT: h;s;WARNING;HARD;1;hm");
        let (_dir, paths) = site("5000 LOG VERSION: 2.0\n5000 SERVICE ALERT: h;s;OK;HARD;1;fine", &[&old]);
        let o = objects(&["s"]);
        // like Livestatus: without state lines at its start, the newer file alone
        // cannot tell what happened before its first line about an object
        let all = compute(&paths, &o, T + 500, T + 6000);
        assert_eq!(spans(&all, "s"), ["500-5000 -1 [UNMONITORED]", "5000-5999 0 [SVC ALERT]"]);
        assert_eq!(spans(&compute(&paths, &o, T + 5000, T + 6000), "s"), ["5000-5999 0 []"]);
        // a period within the older file
        assert_eq!(spans(&compute(&paths, &o, T + 50, T + 200), "s"), ["50-100 0 []", "100-199 1 [SVC ALERT]"]);
        // periods before the first log file are empty
        assert!(compute(&paths, &o, T - 500, T - 100).is_empty());
    }

    #[test]
    fn objects_that_appear_later_were_unmonitored_before() {
        let text = format!(
            "{START}
            0 INITIAL SERVICE STATE: h;ignored;OK;HARD;1;fine
            0 INITIAL SERVICE STATE: elsewhere;s;OK;HARD;1;fine
            700 SERVICE ALERT: h;new;CRITICAL;HARD;1;broken"
        );
        let (_dir, paths) = site(&text, &[]);
        let mut o = objects(&["s", "new", "ignored"]);
        let ignored = o.find("h", "ignored").unwrap();
        o.list[ignored].wanted = false;
        let all = compute(&paths, &o, T, T + 1000);
        assert_eq!(spans(&all, "new"), ["0-700 -1 [UNMONITORED]", "700-999 2 [SVC ALERT]"]);
        // unknown and unwanted objects have no rows
        assert_eq!(all.iter().filter(|s| &*s.service_description != "new").count(), 2);
    }

    #[test]
    fn timeperiod_transitions_set_the_notification_period() {
        let text = "0 LOG VERSION: 2.0
            0 TIMEPERIOD TRANSITION: work;-1;0
            0 TIMEPERIOD TRANSITION: broken;line
            0 logging initial states
            0 INITIAL HOST STATE: h;UP;HARD;1;up
            0 INITIAL SERVICE STATE: h;s;OK;HARD;1;fine
            200 TIMEPERIOD TRANSITION: work;0;1";
        let (_dir, paths) = site(text, &[]);
        let all = compute(&paths, &objects(&["s"]), T, T + 1000);
        assert_eq!(spans(&all, ""), ["0-200 0 out_of_period []", "200-999 0 [TIMEPERIOD]"]);
        // the service has no notification period: always within
        assert_eq!(spans(&all, "s"), ["0-999 0 []"]);
        assert_eq!(&*all[0].notification_period, "work");
    }

    #[test]
    fn lines_out_of_order_and_invalid_utf8() {
        let (dir, paths) = site("", &[]);
        let mut data = log(START).into_bytes();
        data.extend_from_slice(format!("[{}] SERVICE ALERT: h;s;OK;HARD;1;late\n", T + 300).as_bytes());
        data.extend_from_slice(format!("[{}] SERVICE ALERT: h;s;CRITICAL;HARD;1;", T + 100).as_bytes());
        data.extend_from_slice(b"b\xffd\n");
        data.extend_from_slice(b"garbage\n");
        std::fs::write(dir.path().join("nagios.log"), data).unwrap();
        let all = compute(&paths, &objects(&["s"]), T, T + 1000);
        assert_eq!(spans(&all, "s"), ["0-100 0 []", "100-300 2 [SVC ALERT]", "300-999 0 [SVC ALERT]"]);
        assert_eq!(&*all.iter().find(|s| s.state == 2).unwrap().log_output, "b\u{fffd}d");
    }
}
