//! The `statehist` table: state intervals computed from the monitoring history.
//!
//! This is a port of upstream Livestatus' `TableStateHistory`. The log entries of the
//! query period (plus the file containing its start, for the initial states) are
//! replayed in order; every time the state of a host or service changes, the interval
//! since the previous change is emitted as a row. Intervals before the first
//! information about an object are reported with state `-1` ("unmonitored").
//!
//! Computation happens without holding the world lock: only a small snapshot of the
//! object names and their time period names is taken beforehand.
//!
//! Deviation from upstream: when the core logs a shutdown (`Caught SIGTERM, shutting
//! down...`, `Bailing out`), every object becomes unmonitored until it is logged
//! again after the next start, so core downtime shows up as a gap in availability.

use std::collections::HashMap;
use std::sync::Arc;

use corax_core::World;

use super::log::{class, Event, LogChunk, LogEntry};
use super::{getter, Getter, Row};
use crate::value::Value;

const NO_INFORMATION: &str = "No information about that period of time available";

/// Time period names of an object (`notification_period`, `SERVICE_PERIOD`).
#[derive(Debug, Clone, Default)]
struct ObjectInfo {
    notification_period: String,
    service_period: String,
}

/// The bits of the current configuration the computation needs.
#[derive(Debug, Default)]
pub struct Snapshot {
    hosts: HashMap<String, ObjectInfo>,
    services: HashMap<(String, String), ObjectInfo>,
}

impl Snapshot {
    pub fn new(world: &World) -> Self {
        let info = |c: &corax_config::CheckableConfig| ObjectInfo {
            notification_period: c.notification_period.clone().unwrap_or_default(),
            service_period: c.custom_var("SERVICE_PERIOD").unwrap_or_default().to_string(),
        };
        Snapshot {
            hosts: world.hosts.iter().map(|h| (h.cfg.name.clone(), info(&h.cfg.common))).collect(),
            services: world
                .services
                .iter()
                .map(|s| ((world.host(s.host).cfg.name.clone(), s.cfg.description.clone()), info(&s.cfg.common)))
                .collect(),
        }
    }
}

/// One output row (an interval with constant state).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistRow {
    pub time: i64,
    pub lineno: i64,
    pub from: i64,
    pub until: i64,
    /// `until - from` relative to the query period.
    pub duration_part: f64,
    pub state: i64,
    pub host_down: bool,
    pub downtime_depth: i32,
    pub host_downtime_depth: i32,
    pub is_flapping: bool,
    pub in_notification_period: i64,
    pub in_service_period: i64,
    pub notification_period: String,
    pub service_period: String,
    pub debug_info: String,
    pub host_name: String,
    pub service_description: String,
    pub log_output: String,
    pub long_log_output: String,
}

impl HistRow {
    pub fn duration(&self) -> i64 {
        self.until - self.from
    }

    fn duration_in(&self, state: i64) -> i64 {
        if self.state == state {
            self.duration()
        } else {
            0
        }
    }

    fn duration_part_in(&self, state: i64) -> f64 {
        if self.state == state {
            self.duration_part
        } else {
            0.0
        }
    }
}

/// Mutable per-object state during the replay (upstream `HostServiceState`).
#[derive(Debug, Clone, Default)]
struct Hss {
    row: HistRow,
    is_host: bool,
    /// Indexes of the services of this host (hosts only).
    services: Vec<usize>,
    may_no_longer_exist: bool,
    has_vanished: bool,
    last_known_time: i64,
}

/// Values of time periods as announced by `TIMEPERIOD TRANSITION` lines.
#[derive(Default)]
struct TimePeriods(HashMap<String, i64>);

impl TimePeriods {
    fn find(&self, name: &str) -> i64 {
        self.0.get(name).copied().unwrap_or(1)
    }
}

/// `name;from;to` of a `TIMEPERIOD TRANSITION` line.
fn parse_transition(options: &str) -> Option<(String, i64, i64)> {
    let mut f = options.split(';');
    let (name, from, to) = (f.next()?, f.next()?, f.next()?);
    if f.next().is_some() {
        return None;
    }
    Some((name.to_string(), from.trim().parse().ok()?, to.trim().parse().ok()?))
}

#[derive(PartialEq, Eq)]
enum Modification {
    Changed,
    Unchanged,
}

struct Replay<'s> {
    snap: &'s Snapshot,
    since: i64,
    until: i64,
    states: Vec<Hss>,
    index: HashMap<(String, String), usize>,
    out: Vec<HistRow>,
    only_update: bool,
    time_periods: TimePeriods,
}

impl Replay<'_> {
    /// Emit the current interval of `idx` (if non-empty) and start a new one.
    fn process(&mut self, idx: usize) {
        let period = (self.until - self.since) as f64;
        let hss = &mut self.states[idx];
        let r = &mut hss.row;
        if r.until - r.from != 0 {
            r.duration_part = (r.until - r.from) as f64 / period;
            self.out.push(r.clone());
        }
        r.from = r.until;
    }

    fn emit(&mut self, idx: usize) {
        if !self.only_update {
            self.process(idx);
        } else {
            let r = &mut self.states[idx].row;
            r.from = r.until;
        }
    }

    fn set_unknown_to_unmonitored(&mut self) {
        for h in &mut self.states {
            if h.may_no_longer_exist {
                h.has_vanished = true;
            }
        }
    }

    fn state_for_entry(&mut self, e: &LogEntry) -> Option<usize> {
        let is_host = e.service_description.is_empty();
        let info = if is_host {
            self.snap.hosts.get(&e.host_name)?
        } else {
            self.snap.services.get(&(e.host_name.clone(), e.service_description.clone()))?
        };
        let key = (e.host_name.clone(), e.service_description.clone());
        if let Some(&idx) = self.index.get(&key) {
            return Some(idx);
        }
        let idx = self.states.len();
        let mut hss = Hss {
            is_host,
            row: HistRow {
                host_name: e.host_name.clone(),
                service_description: e.service_description.clone(),
                from: self.since,
                log_output: NO_INFORMATION.to_string(),
                notification_period: info.notification_period.clone(),
                service_period: info.service_period.clone(),
                in_notification_period: self.time_periods.find(&info.notification_period),
                in_service_period: self.time_periods.find(&info.service_period),
                ..Default::default()
            },
            ..Default::default()
        };
        if is_host {
            hss.services = self
                .states
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_host && s.row.host_name == e.host_name)
                .map(|(i, _)| i)
                .collect();
        } else if let Some(&host_idx) = self.index.get(&(e.host_name.clone(), String::new())) {
            self.states[host_idx].services.push(idx);
            hss.row.host_downtime_depth = self.states[host_idx].row.host_downtime_depth;
            hss.row.host_down = self.states[host_idx].row.host_down;
        }
        if !self.only_update && e.time - hss.row.from > 600 {
            hss.row.debug_info = "UNMONITORED ".into();
            hss.row.state = -1;
        }
        self.states.push(hss);
        self.index.insert(key, idx);
        Some(idx)
    }

    fn handle_state_entry(&mut self, e: &LogEntry) {
        let Some(idx) = self.state_for_entry(e) else { return };
        let changed = self.update(idx, e);
        if matches!(e.event, Event::HostAlert | Event::CurrentHostState | Event::HostDowntimeAlert)
            && changed == Modification::Changed
        {
            for svc in self.states[idx].services.clone() {
                self.update(svc, e);
            }
        }
    }

    /// Upstream `updateHostServiceState`.
    fn update(&mut self, idx: usize, e: &LogEntry) -> Modification {
        let mut changed = Modification::Changed;
        if self.states[idx].has_vanished {
            let t = self.states[idx].last_known_time;
            self.states[idx].row.time = t;
            self.states[idx].row.until = t;
            self.emit(idx);
            let notif = self.time_periods.find(&self.states[idx].row.notification_period);
            let svc_period = self.time_periods.find(&self.states[idx].row.service_period);
            let h = &mut self.states[idx];
            h.may_no_longer_exist = false;
            h.has_vanished = false;
            h.row.state = -1;
            h.row.debug_info = "UNMONITORED".into();
            h.row.downtime_depth = 0;
            h.row.is_flapping = false;
            h.row.log_output.clear();
            h.row.long_log_output.clear();
            h.row.in_notification_period = notif;
            h.row.in_service_period = svc_period;
        }
        {
            let h = &mut self.states[idx];
            h.row.time = e.time;
            h.row.lineno = e.lineno;
            h.row.until = e.time;
            h.may_no_longer_exist = false;
        }
        let started = e.state_type.starts_with("STARTED");
        match e.event {
            Event::CurrentHostState | Event::InitialHostState | Event::HostAlert => {
                let down = e.state > 0;
                if self.states[idx].is_host {
                    if self.states[idx].row.state != e.state {
                        self.emit(idx);
                        let r = &mut self.states[idx].row;
                        r.state = e.state;
                        r.host_down = down;
                        r.debug_info = "HOST STATE".into();
                    } else {
                        changed = Modification::Unchanged;
                    }
                } else if self.states[idx].row.host_down != down {
                    self.emit(idx);
                    let r = &mut self.states[idx].row;
                    r.host_down = down;
                    r.debug_info = "SVC HOST STATE".into();
                }
            }
            Event::CurrentServiceState | Event::InitialServiceState | Event::ServiceAlert => {
                if self.states[idx].row.state != e.state {
                    self.emit(idx);
                    let r = &mut self.states[idx].row;
                    r.debug_info = "SVC ALERT".into();
                    r.state = e.state;
                }
            }
            Event::HostDowntimeAlert => {
                if (self.states[idx].row.host_downtime_depth > 0) != started {
                    self.emit(idx);
                    let is_host = self.states[idx].is_host;
                    self.states[idx].row.debug_info =
                        if is_host { "HOST DOWNTIME" } else { "SVC HOST DOWNTIME" }.into();
                }
                let delta = if started { 1 } else { -1 };
                let h = &mut self.states[idx];
                h.row.host_downtime_depth += delta;
                if h.is_host {
                    h.row.downtime_depth += delta;
                }
            }
            Event::ServiceDowntimeAlert => {
                if (self.states[idx].row.downtime_depth > 0) != started {
                    self.emit(idx);
                    self.states[idx].row.debug_info = "DOWNTIME SERVICE".into();
                }
                self.states[idx].row.downtime_depth += if started { 1 } else { -1 };
            }
            Event::HostFlappingAlert | Event::ServiceFlappingAlert => {
                if self.states[idx].row.is_flapping != started {
                    self.emit(idx);
                    let r = &mut self.states[idx].row;
                    r.debug_info = "FLAPPING ".into();
                    r.is_flapping = started;
                } else {
                    changed = Modification::Unchanged;
                }
            }
            _ => {}
        }

        let initial = matches!(e.event, Event::InitialHostState | Event::InitialServiceState);
        let output = if initial && e.plugin_output == "(null)" { "" } else { e.plugin_output.as_str() };
        let r = &mut self.states[idx].row;
        if !output.is_empty() || r.log_output.is_empty() {
            r.log_output = output.to_string();
        }
        if !e.long_plugin_output.is_empty() || r.long_log_output.is_empty() {
            r.long_log_output = e.long_plugin_output.clone();
        }
        changed
    }

    fn handle_timeperiod_transition(&mut self, e: &LogEntry) {
        let Some((name, _from, to)) = parse_transition(&e.options) else {
            tracing::warn!("invalid TIMEPERIOD TRANSITION: {}", e.message);
            return;
        };
        self.time_periods.0.insert(name.clone(), to);
        for idx in 0..self.states.len() {
            {
                let r = &mut self.states[idx].row;
                r.time = e.time;
                r.lineno = e.lineno;
                r.until = e.time;
            }
            if self.states[idx].row.notification_period == name && to != self.states[idx].row.in_notification_period {
                self.emit(idx);
                let r = &mut self.states[idx].row;
                r.debug_info = "TIMEPERIOD ".into();
                r.in_notification_period = to;
            }
            if self.states[idx].row.service_period == name && to != self.states[idx].row.in_service_period {
                self.emit(idx);
                let r = &mut self.states[idx].row;
                r.debug_info = "TIMEPERIOD ".into();
                r.in_service_period = to;
            }
        }
    }

    fn handle_logging_initial_states(&mut self, time: i64, rotated: bool) {
        for h in &mut self.states {
            if !h.has_vanished {
                h.last_known_time = time;
                h.may_no_longer_exist = true;
            }
        }
        if rotated {
            // reset downtime depths
            for idx in 0..self.states.len() {
                let r = &self.states[idx].row;
                if r.downtime_depth != 0 || r.host_downtime_depth != 0 {
                    self.states[idx].row.until = time;
                    self.emit(idx);
                    let r = &mut self.states[idx].row;
                    r.debug_info = "RESET DOWNTIME DEPTHS".into();
                    r.downtime_depth = 0;
                    r.host_downtime_depth = 0;
                }
            }
        }
    }

    /// corax extension: everything is unmonitored while the core is down.
    fn handle_core_stopping(&mut self, e: &LogEntry) {
        for idx in 0..self.states.len() {
            if self.states[idx].row.state == -1 {
                continue;
            }
            {
                let r = &mut self.states[idx].row;
                r.time = e.time;
                r.lineno = e.lineno;
                r.until = e.time;
            }
            self.emit(idx);
            let r = &mut self.states[idx].row;
            r.state = -1;
            r.debug_info = "CORE STOPPED".into();
            r.log_output.clear();
            r.long_log_output.clear();
        }
    }

    fn final_reports(&mut self) {
        for idx in 0..self.states.len() {
            if self.states[idx].may_no_longer_exist {
                let t = self.states[idx].last_known_time;
                self.states[idx].row.time = t;
                self.states[idx].row.until = t;
                self.process(idx);
                let r = &mut self.states[idx].row;
                r.state = -1;
                r.debug_info = "UNMONITORED".into();
                r.log_output.clear();
                r.long_log_output.clear();
            }
            let r = &mut self.states[idx].row;
            r.time = self.until - 1;
            r.until = r.time;
            self.process(idx);
        }
    }
}

/// Replay `entries` (chronological) for the period `[since, until)`.
pub fn compute<'e>(
    entries: impl Iterator<Item = &'e LogEntry>,
    snap: &Snapshot,
    since: i64,
    until: i64,
) -> Vec<HistRow> {
    if since >= until {
        return Vec::new();
    }
    let mut r = Replay {
        snap,
        since,
        until,
        states: Vec::new(),
        index: HashMap::new(),
        out: Vec::new(),
        only_update: true,
        time_periods: TimePeriods::default(),
    };
    let mut in_initial_states = false;
    let mut rotated = false;
    for e in entries.filter(|e| matches!(e.class, class::ALERT | class::PROGRAM | class::STATE)) {
        if e.time >= until {
            break;
        }
        if r.only_update && e.time >= since {
            for h in &mut r.states {
                h.row.from = since;
                h.row.until = since;
            }
            r.only_update = false;
        }
        let leave_initial = |r: &mut Replay<'_>, in_initial: &mut bool| {
            if *in_initial {
                r.set_unknown_to_unmonitored();
            }
            *in_initial = false;
        };
        match e.event {
            Event::LogVersion => {
                rotated = true;
                leave_initial(&mut r, &mut in_initial_states);
            }
            Event::CoreStarting => {
                rotated = false;
                leave_initial(&mut r, &mut in_initial_states);
            }
            Event::CoreStopping => {
                leave_initial(&mut r, &mut in_initial_states);
                r.handle_core_stopping(e);
            }
            Event::None | Event::HostAcknowledgeAlert | Event::ServiceAcknowledgeAlert => {
                leave_initial(&mut r, &mut in_initial_states)
            }
            Event::InitialHostState | Event::InitialServiceState => r.handle_state_entry(e),
            Event::HostAlert
            | Event::CurrentHostState
            | Event::HostDowntimeAlert
            | Event::HostFlappingAlert
            | Event::ServiceAlert
            | Event::CurrentServiceState
            | Event::ServiceDowntimeAlert
            | Event::ServiceFlappingAlert => {
                leave_initial(&mut r, &mut in_initial_states);
                r.handle_state_entry(e);
            }
            Event::TimeperiodTransition => {
                leave_initial(&mut r, &mut in_initial_states);
                r.handle_timeperiod_transition(e);
            }
            Event::LoggingInitialStates => {
                leave_initial(&mut r, &mut in_initial_states);
                r.handle_logging_initial_states(e.time, rotated);
                in_initial_states = true;
            }
        }
    }
    r.final_reports();
    r.out
}

/// Compute the rows for loaded log chunks.
pub fn compute_from_chunks(chunks: &[Arc<LogChunk>], snap: &Snapshot, since: i64, until: i64) -> Vec<HistRow> {
    compute(chunks.iter().flat_map(|c| c.entries.iter()), snap, since, until)
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
        "time" => own!(x => Value::Int(x.time)),
        "lineno" => own!(x => Value::Int(x.lineno)),
        "from" => own!(x => Value::Int(x.from)),
        "until" => own!(x => Value::Int(x.until)),
        "duration" => own!(x => Value::Int(x.duration())),
        "duration_part" => own!(x => Value::Float(x.duration_part)),
        "state" => own!(x => Value::Int(x.state)),
        "host_down" => own!(x => Value::bool(x.host_down)),
        "in_downtime" => own!(x => Value::bool(x.downtime_depth > 0)),
        "in_host_downtime" => own!(x => Value::bool(x.host_downtime_depth > 0)),
        "is_flapping" => own!(x => Value::bool(x.is_flapping)),
        "in_notification_period" => own!(x => Value::Int(x.in_notification_period)),
        "notification_period" => own!(x => Value::str(&x.notification_period)),
        "in_service_period" => own!(x => Value::Int(x.in_service_period)),
        "service_period" => own!(x => Value::str(&x.service_period)),
        "debug_info" => own!(x => Value::str(&x.debug_info)),
        "host_name" => own!(x => Value::str(&x.host_name)),
        "service_description" => own!(x => Value::str(&x.service_description)),
        "log_output" => own!(x => Value::str(&x.log_output)),
        "long_log_output" => own!(x => Value::str(&x.long_log_output)),
        "duration_ok" => own!(x => Value::Int(x.duration_in(0))),
        "duration_warning" => own!(x => Value::Int(x.duration_in(1))),
        "duration_critical" => own!(x => Value::Int(x.duration_in(2))),
        "duration_unknown" => own!(x => Value::Int(x.duration_in(3))),
        "duration_unmonitored" => own!(x => Value::Int(x.duration_in(-1))),
        "duration_part_ok" => own!(x => Value::Float(x.duration_part_in(0))),
        "duration_part_warning" => own!(x => Value::Float(x.duration_part_in(1))),
        "duration_part_critical" => own!(x => Value::Float(x.duration_part_in(2))),
        "duration_part_unknown" => own!(x => Value::Float(x.duration_part_in(3))),
        "duration_part_unmonitored" => own!(x => Value::Float(x.duration_part_in(-1))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        s.hosts.insert("h1".into(), ObjectInfo { notification_period: "24X7".into(), ..Default::default() });
        s.services.insert(
            ("h1".into(), "svc".into()),
            ObjectInfo { notification_period: "24X7".into(), ..Default::default() },
        );
        s
    }

    fn entries(text: &str) -> Vec<LogEntry> {
        text.lines().enumerate().filter_map(|(i, l)| LogEntry::parse(i as i64 + 1, l)).collect()
    }

    fn rows(text: &str, since: i64, until: i64) -> Vec<HistRow> {
        let e = entries(text);
        compute(e.iter(), &snap(), since, until)
    }

    fn brief(rows: &[HistRow], host: &str, svc: &str) -> Vec<(i64, i64, i64)> {
        let mut v: Vec<_> = rows
            .iter()
            .filter(|r| r.host_name == host && r.service_description == svc)
            .map(|r| (r.from, r.until, r.state))
            .collect();
        v.sort();
        v
    }

    const LOG: &str = "\
[1700000000] LOG VERSION: 2.0
[1700000000] corax 0.1.0 starting... (PID=1)
[1700000000] INITIAL HOST STATE: h1;UP;HARD;1;PING OK
[1700000000] INITIAL SERVICE STATE: h1;svc;OK;HARD;1;(null)
[1700000100] SERVICE ALERT: h1;svc;CRITICAL;HARD;1;disk full
[1700000200] SERVICE ALERT: h1;svc;OK;HARD;1;fine again
[1700000300] HOST DOWNTIME ALERT: h1;STARTED; Host has entered a period of scheduled downtime
[1700000400] HOST DOWNTIME ALERT: h1;STOPPED; Host has exited from a period of scheduled downtime
[1700000500] Caught SIGTERM, shutting down...
[1700000500] Successfully shutdown... (PID=1)
[1700000700] corax 0.1.0 starting... (PID=2)
[1700000700] INITIAL HOST STATE: h1;UP;HARD;1;PING OK
[1700000700] INITIAL SERVICE STATE: h1;svc;OK;HARD;1;fine
[1700000800] HOST ALERT: h1;DOWN;HARD;1;timeout
";

    #[test]
    fn intervals_and_durations() {
        let r = rows(LOG, 1_700_000_000, 1_700_001_000);
        assert_eq!(
            brief(&r, "h1", "svc"),
            vec![
                (1_700_000_000, 1_700_000_100, 0),
                (1_700_000_100, 1_700_000_200, 2),
                (1_700_000_200, 1_700_000_300, 0),
                (1_700_000_300, 1_700_000_400, 0),
                (1_700_000_400, 1_700_000_500, 0),
                (1_700_000_500, 1_700_000_700, -1),
                (1_700_000_700, 1_700_000_800, 0),
                (1_700_000_800, 1_700_000_999, 0),
            ]
        );
        let svc: Vec<&HistRow> = r.iter().filter(|r| r.service_description == "svc").collect();
        let crit = svc.iter().find(|r| r.state == 2).unwrap();
        assert_eq!(crit.duration(), 100);
        assert_eq!(crit.duration_in(2), 100);
        assert!((crit.duration_part - 0.1).abs() < 1e-9);
        assert_eq!(crit.log_output, "disk full");
        let in_dt = svc.iter().find(|r| r.from == 1_700_000_300).unwrap();
        assert!(in_dt.host_downtime_depth > 0 && in_dt.downtime_depth == 0);
        let last = svc.iter().find(|r| r.from == 1_700_000_800).unwrap();
        assert!(last.host_down);
        let gap = svc.iter().find(|r| r.state == -1).unwrap();
        assert_eq!(gap.duration_in(-1), 200);

        assert_eq!(
            brief(&r, "h1", ""),
            vec![
                (1_700_000_000, 1_700_000_300, 0),
                (1_700_000_300, 1_700_000_400, 0),
                (1_700_000_400, 1_700_000_500, 0),
                (1_700_000_500, 1_700_000_700, -1),
                (1_700_000_700, 1_700_000_800, 0),
                (1_700_000_800, 1_700_000_999, 1),
            ]
        );
    }

    #[test]
    fn no_information_before_first_entry() {
        // query starts 12h before the first log line: unmonitored with "no information"
        let r = rows(LOG, 1_699_956_800, 1_700_000_100);
        let first = r.iter().find(|r| r.host_name == "h1" && r.service_description.is_empty()).unwrap();
        assert_eq!((first.from, first.until, first.state), (1_699_956_800, 1_700_000_000, -1));
        assert_eq!(first.log_output, NO_INFORMATION);
        let svc_ok = r.iter().find(|r| r.service_description == "svc" && r.state == 0).unwrap();
        // "(null)" output of the initial state keeps the previous text
        assert_eq!(svc_ok.log_output, NO_INFORMATION);
    }

    #[test]
    fn period_inside_history() {
        // states before the period are replayed without output
        let r = rows(LOG, 1_700_000_150, 1_700_000_250);
        assert_eq!(brief(&r, "h1", "svc"), vec![(1_700_000_150, 1_700_000_200, 2), (1_700_000_200, 1_700_000_249, 0)]);
        assert_eq!(brief(&r, "h1", ""), vec![(1_700_000_150, 1_700_000_249, 0)]);
    }

    #[test]
    fn unknown_objects_and_empty_period() {
        let r = rows("[1700000000] SERVICE ALERT: other;x;OK;HARD;1;ok\n", 1_700_000_000, 1_700_000_100);
        assert!(r.is_empty());
        assert!(rows(LOG, 10, 10).is_empty());
    }

    #[test]
    fn timeperiod_transitions() {
        let log = "\
[1700000000] INITIAL HOST STATE: h1;UP;HARD;1;ok
[1700000050] TIMEPERIOD TRANSITION: 24X7;1;0
[1700000080] TIMEPERIOD TRANSITION: 24X7;0;1
";
        let r = rows(log, 1_700_000_000, 1_700_000_100);
        let v: Vec<(i64, i64, i64)> = r.iter().map(|r| (r.from, r.until, r.in_notification_period)).collect();
        assert_eq!(
            v,
            vec![
                (1_700_000_000, 1_700_000_050, 1),
                (1_700_000_050, 1_700_000_080, 0),
                (1_700_000_080, 1_700_000_099, 1)
            ]
        );
    }
}
