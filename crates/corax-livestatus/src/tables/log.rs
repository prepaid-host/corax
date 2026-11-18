//! The `log` table: parses `nagios.log` and the archive directory.
//!
//! Parsed files are cached. The current log file is parsed incrementally (only the
//! bytes appended since the last query), archive files are parsed once and evicted
//! LRU-style when the cache grows too large.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use corax_core::Paths;

use super::{getter, Getter, Row};
use crate::value::Value;

/// Livestatus log classes.
pub mod class {
    pub const INFO: i64 = 0;
    pub const ALERT: i64 = 1;
    pub const PROGRAM: i64 = 2;
    pub const NOTIFICATION: i64 = 3;
    pub const PASSIVECHECK: i64 = 4;
    pub const EXT_COMMAND: i64 = 5;
    pub const STATE: i64 = 6;
    pub const ALERT_HANDLERS: i64 = 8;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Param {
    HostName,
    ServiceDescription,
    CommandName,
    CommandNameWithWorkaround,
    ContactName,
    HostState,
    ServiceState,
    ExitCode,
    State,
    StateType,
    Attempt,
    Comment,
    PluginOutput,
    LongPluginOutput,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    HostState,
    ServiceState,
    HostOther,
    ServiceOther,
    TimeperiodTransition,
}

struct LogDef {
    prefix: &'static str,
    class: i64,
    kind: Kind,
    params: &'static [Param],
}

use Param::*;

const DEFS: &[LogDef] = &[
    LogDef {
        prefix: "INITIAL HOST STATE",
        class: class::STATE,
        kind: Kind::HostState,
        params: &[HostName, HostState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "CURRENT HOST STATE",
        class: class::STATE,
        kind: Kind::HostState,
        params: &[HostName, HostState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "HOST ALERT",
        class: class::ALERT,
        kind: Kind::HostState,
        params: &[HostName, HostState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "HOST DOWNTIME ALERT",
        class: class::ALERT,
        kind: Kind::HostOther,
        params: &[HostName, StateType, Comment],
    },
    LogDef {
        prefix: "HOST ACKNOWLEDGE ALERT",
        class: class::ALERT,
        kind: Kind::HostOther,
        params: &[HostName, StateType, ContactName, Comment],
    },
    LogDef {
        prefix: "HOST FLAPPING ALERT",
        class: class::ALERT,
        kind: Kind::HostOther,
        params: &[HostName, StateType, Comment],
    },
    LogDef {
        prefix: "INITIAL SERVICE STATE",
        class: class::STATE,
        kind: Kind::ServiceState,
        params: &[HostName, ServiceDescription, ServiceState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "CURRENT SERVICE STATE",
        class: class::STATE,
        kind: Kind::ServiceState,
        params: &[HostName, ServiceDescription, ServiceState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "SERVICE ALERT",
        class: class::ALERT,
        kind: Kind::ServiceState,
        params: &[HostName, ServiceDescription, ServiceState, StateType, Attempt, PluginOutput, LongPluginOutput],
    },
    LogDef {
        prefix: "SERVICE DOWNTIME ALERT",
        class: class::ALERT,
        kind: Kind::ServiceOther,
        params: &[HostName, ServiceDescription, StateType, Comment],
    },
    LogDef {
        prefix: "SERVICE ACKNOWLEDGE ALERT",
        class: class::ALERT,
        kind: Kind::ServiceOther,
        params: &[HostName, ServiceDescription, StateType, ContactName, Comment],
    },
    LogDef {
        prefix: "SERVICE FLAPPING ALERT",
        class: class::ALERT,
        kind: Kind::ServiceOther,
        params: &[HostName, ServiceDescription, StateType, Comment],
    },
    LogDef {
        prefix: "TIMEPERIOD TRANSITION",
        class: class::STATE,
        kind: Kind::TimeperiodTransition,
        params: &[Ignore, Ignore, Ignore],
    },
    LogDef {
        prefix: "HOST NOTIFICATION",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[
            ContactName,
            HostName,
            StateType,
            CommandNameWithWorkaround,
            PluginOutput,
            Ignore,
            Comment,
            LongPluginOutput,
        ],
    },
    LogDef {
        prefix: "SERVICE NOTIFICATION",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[
            ContactName,
            HostName,
            ServiceDescription,
            StateType,
            CommandNameWithWorkaround,
            PluginOutput,
            Ignore,
            Comment,
            LongPluginOutput,
        ],
    },
    LogDef {
        prefix: "HOST NOTIFICATION RESULT",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[ContactName, HostName, StateType, CommandNameWithWorkaround, PluginOutput, Comment],
    },
    LogDef {
        prefix: "SERVICE NOTIFICATION RESULT",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[
            ContactName,
            HostName,
            ServiceDescription,
            StateType,
            CommandNameWithWorkaround,
            PluginOutput,
            Comment,
        ],
    },
    LogDef {
        prefix: "HOST NOTIFICATION PROGRESS",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[ContactName, HostName, StateType, CommandNameWithWorkaround, PluginOutput],
    },
    LogDef {
        prefix: "SERVICE NOTIFICATION PROGRESS",
        class: class::NOTIFICATION,
        kind: Kind::None,
        params: &[ContactName, HostName, ServiceDescription, StateType, CommandNameWithWorkaround, PluginOutput],
    },
    LogDef {
        prefix: "HOST ALERT HANDLER STARTED",
        class: class::ALERT_HANDLERS,
        kind: Kind::None,
        params: &[HostName, CommandName],
    },
    LogDef {
        prefix: "SERVICE ALERT HANDLER STARTED",
        class: class::ALERT_HANDLERS,
        kind: Kind::None,
        params: &[HostName, ServiceDescription, CommandName],
    },
    LogDef {
        prefix: "HOST ALERT HANDLER STOPPED",
        class: class::ALERT_HANDLERS,
        kind: Kind::None,
        params: &[HostName, CommandName, ExitCode, PluginOutput],
    },
    LogDef {
        prefix: "SERVICE ALERT HANDLER STOPPED",
        class: class::ALERT_HANDLERS,
        kind: Kind::None,
        params: &[HostName, ServiceDescription, CommandName, ExitCode, PluginOutput],
    },
    LogDef {
        prefix: "PASSIVE SERVICE CHECK",
        class: class::PASSIVECHECK,
        kind: Kind::None,
        params: &[HostName, ServiceDescription, State, PluginOutput],
    },
    LogDef {
        prefix: "PASSIVE HOST CHECK",
        class: class::PASSIVECHECK,
        kind: Kind::None,
        params: &[HostName, State, PluginOutput],
    },
    LogDef { prefix: "EXTERNAL COMMAND", class: class::EXT_COMMAND, kind: Kind::None, params: &[] },
];

/// What a log line means for the state history (upstream `LogEntryKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Event {
    #[default]
    None,
    LogVersion,
    LoggingInitialStates,
    CoreStarting,
    CoreStopping,
    InitialHostState,
    CurrentHostState,
    HostAlert,
    HostDowntimeAlert,
    HostAcknowledgeAlert,
    HostFlappingAlert,
    InitialServiceState,
    CurrentServiceState,
    ServiceAlert,
    ServiceDowntimeAlert,
    ServiceAcknowledgeAlert,
    ServiceFlappingAlert,
    TimeperiodTransition,
}

impl Event {
    fn from_prefix(prefix: &str) -> Event {
        match prefix {
            "INITIAL HOST STATE" => Event::InitialHostState,
            "CURRENT HOST STATE" => Event::CurrentHostState,
            "HOST ALERT" => Event::HostAlert,
            "HOST DOWNTIME ALERT" => Event::HostDowntimeAlert,
            "HOST ACKNOWLEDGE ALERT" => Event::HostAcknowledgeAlert,
            "HOST FLAPPING ALERT" => Event::HostFlappingAlert,
            "INITIAL SERVICE STATE" => Event::InitialServiceState,
            "CURRENT SERVICE STATE" => Event::CurrentServiceState,
            "SERVICE ALERT" => Event::ServiceAlert,
            "SERVICE DOWNTIME ALERT" => Event::ServiceDowntimeAlert,
            "SERVICE ACKNOWLEDGE ALERT" => Event::ServiceAcknowledgeAlert,
            "SERVICE FLAPPING ALERT" => Event::ServiceFlappingAlert,
            "TIMEPERIOD TRANSITION" => Event::TimeperiodTransition,
            _ => Event::None,
        }
    }
}

/// One parsed log line.
#[derive(Debug, Clone, Default)]
pub struct LogEntry {
    pub time: i64,
    pub lineno: i64,
    pub message: String,
    pub class: i64,
    kind: Option<Kind>,
    /// Fine-grained classification used by the `statehist` table.
    pub event: Event,
    pub type_: String,
    pub options: String,
    pub state: i64,
    pub state_type: String,
    pub attempt: i64,
    pub host_name: String,
    pub service_description: String,
    pub contact_name: String,
    pub command_name: String,
    pub plugin_output: String,
    pub long_plugin_output: String,
    pub comment: String,
}

fn strip_parens(s: &str) -> &str {
    if s.ends_with(')') {
        if let Some(l) = s.rfind('(') {
            return &s[l + 1..s.len() - 1];
        }
    }
    s
}

fn parse_service_state(s: &str) -> i64 {
    match strip_parens(s) {
        "WARNING" => 1,
        "CRITICAL" => 2,
        "UNKNOWN" => 3,
        _ => 0,
    }
}

fn parse_host_state(s: &str) -> i64 {
    match strip_parens(s) {
        "DOWN" | "WARNING" => 1,
        "UNREACHABLE" | "CRITICAL" => 2,
        "UNKNOWN" => 3,
        _ => 0,
    }
}

/// External commands whose target is a host / service (for `host_name` etc.).
fn command_target(cmd: &str) -> u8 {
    if cmd == "DEL_DOWNTIME_BY_HOST_NAME" {
        return 2;
    }
    let known = cmd.starts_with("ACKNOWLEDGE_")
        || cmd.starts_with("ADD_")
        || cmd.starts_with("CHANGE_")
        || cmd.starts_with("DELAY_")
        || cmd.starts_with("DEL_ALL_")
        || cmd.starts_with("DISABLE_")
        || cmd.starts_with("ENABLE_")
        || cmd.starts_with("PROCESS_")
        || cmd.starts_with("REMOVE_")
        || cmd.starts_with("SCHEDULE_")
        || cmd.starts_with("SEND_CUSTOM_")
        || cmd.starts_with("SET_")
        || cmd.starts_with("START_OBSESSING")
        || cmd.starts_with("STOP_OBSESSING")
        || cmd == "MK_LOGWATCH_ACKNOWLEDGE"
        || cmd == "EC_DELETE_EVENTS_OF_HOST";
    if !known
        || !(cmd.contains("HOST") || cmd.contains("SVC") || cmd.contains("SERVICE") || cmd == "MK_LOGWATCH_ACKNOWLEDGE")
    {
        return 0;
    }
    if (cmd.contains("SVC") || cmd.contains("SERVICE")) && !cmd.contains("HOST") {
        2
    } else {
        1
    }
}

/// Classify core program messages (upstream `LogEntry::classifyLogMessage`).
fn program_event(text: &str) -> Option<Event> {
    if text.starts_with("LOG VERSION: 2.0") {
        return Some(Event::LogVersion);
    }
    if text.starts_with("logging initial states") || text.starts_with("logging intitial states") {
        return Some(Event::LoggingInitialStates);
    }
    let ellipsis = text.contains("...");
    if ellipsis && (text.contains("starting...") || text.contains("active mode...")) {
        return Some(Event::CoreStarting);
    }
    if text.contains("Bailing out")
        || (ellipsis && (text.contains("shutting down...") || text.contains("standby mode...")))
    {
        return Some(Event::CoreStopping);
    }
    None
}

impl LogEntry {
    /// Parse `[1700000000] TYPE: options`. Returns `None` for malformed lines.
    pub fn parse(lineno: i64, line: &str) -> Option<Self> {
        let b = line.as_bytes();
        if b.len() < 13 || b[0] != b'[' || b[11] != b']' || b[12] != b' ' {
            return None;
        }
        let time: i64 = line[1..11].parse().ok()?;
        let text = &line[13..];
        let options = match line.find(':') {
            Some(p) => line[p + 1..].trim_start_matches(' ').to_string(),
            None => String::new(),
        };
        let mut e = LogEntry { time, lineno, message: line.to_string(), options, ..Default::default() };

        if let Some(def) = DEFS.iter().find(|d| text.starts_with(d.prefix) && text[d.prefix.len()..].starts_with(": "))
        {
            e.type_ = def.prefix.to_string();
            e.class = def.class;
            e.kind = Some(def.kind);
            e.event = Event::from_prefix(def.prefix);
            let mut rest = Some(&text[def.prefix.len() + 2..]);
            for &p in def.params {
                let field = match rest {
                    Some(r) => match r.split_once(';') {
                        Some((f, tail)) => {
                            rest = Some(tail);
                            f
                        }
                        None => {
                            rest = None;
                            r
                        }
                    },
                    None => "",
                };
                e.assign(p, field);
            }
            if e.class == class::EXT_COMMAND {
                e.set_command_target(&text[def.prefix.len() + 2..]);
            }
            return Some(e);
        }

        e.type_ = text.to_string();
        e.kind = Some(Kind::None);
        if let Some(cmd) = text.strip_prefix("Warning: Unrecognized external command -> ") {
            e.class = class::EXT_COMMAND;
            e.set_command_target(cmd);
        } else if let Some(event) = program_event(text) {
            e.class = class::PROGRAM;
            e.event = event;
        } else {
            e.class = class::INFO;
        }
        Some(e)
    }

    fn set_command_target(&mut self, cmd: &str) {
        let mut parts = cmd.split(';');
        let name = parts.next().unwrap_or("");
        match command_target(name) {
            1 => self.host_name = parts.next().unwrap_or("").to_string(),
            2 => {
                self.host_name = parts.next().unwrap_or("").to_string();
                self.service_description = parts.next().unwrap_or("").to_string();
            }
            _ => {}
        }
    }

    fn assign(&mut self, p: Param, field: &str) {
        match p {
            HostName => self.host_name = field.to_string(),
            ServiceDescription => self.service_description = field.to_string(),
            CommandName => self.command_name = field.to_string(),
            CommandNameWithWorkaround => {
                self.command_name = field.to_string();
                if self.state_type.is_empty() {
                    return;
                }
                if self.state_type == "check-mk-notify" {
                    std::mem::swap(&mut self.state_type, &mut self.command_name);
                    if self.state_type.is_empty() {
                        return;
                    }
                }
                self.state = if self.service_description.is_empty() {
                    parse_host_state(&self.state_type)
                } else {
                    parse_service_state(&self.state_type)
                };
            }
            ContactName => self.contact_name = field.to_string(),
            HostState => self.state = parse_host_state(field),
            ServiceState | ExitCode => self.state = parse_service_state(field),
            State => self.state = field.trim().parse().unwrap_or(0),
            StateType => self.state_type = field.to_string(),
            Attempt => self.attempt = field.trim().parse().unwrap_or(0),
            Comment => self.comment = field.to_string(),
            PluginOutput => self.plugin_output = field.to_string(),
            LongPluginOutput => self.long_plugin_output = field.replace("\\n", "\n"),
            Ignore => {}
        }
    }

    pub fn state_info(&self) -> String {
        let host_state = |s: i64| match s {
            0 => "UP".to_string(),
            1 => "DOWN".to_string(),
            2 => "UNREACHABLE".to_string(),
            n => format!("FUNNY_HOST_STATE_{n}"),
        };
        let svc_state = |s: i64| match s {
            0 => "OK".to_string(),
            1 => "WARNING".to_string(),
            2 => "CRITICAL".to_string(),
            3 => "UNKNOWN".to_string(),
            n => format!("FUNNY_HOST_STATE_{n}"),
        };
        let exit_code = |s: i64| match s {
            0 => "SUCCESS".to_string(),
            1 => "TEMPORARY_FAILURE".to_string(),
            2 => "PERMANENT_FAILURE".to_string(),
            n => format!("FUNNY_EXIT_CODE_{n}"),
        };
        let parens = |a: &str, b: String| format!("{a} ({b})");
        match self.kind {
            Some(Kind::HostState) => parens(&self.state_type, host_state(self.state)),
            Some(Kind::ServiceState) => parens(&self.state_type, svc_state(self.state)),
            Some(Kind::HostOther) | Some(Kind::ServiceOther) => self.state_type.clone(),
            Some(Kind::TimeperiodTransition) | None => String::new(),
            Some(Kind::None) => match self.type_.as_str() {
                "HOST NOTIFICATION RESULT"
                | "SERVICE NOTIFICATION RESULT"
                | "HOST NOTIFICATION PROGRESS"
                | "SERVICE NOTIFICATION PROGRESS"
                | "HOST ALERT HANDLER STOPPED"
                | "SERVICE ALERT HANDLER STOPPED" => parens("EXIT_CODE", exit_code(self.state)),
                "HOST NOTIFICATION" | "SERVICE NOTIFICATION" => {
                    let plain: &[&str] = if self.type_.starts_with("HOST") {
                        &["UP", "DOWN", "UNREACHABLE"]
                    } else {
                        &["OK", "WARNING", "CRITICAL", "UNKNOWN"]
                    };
                    if plain.contains(&self.state_type.as_str()) {
                        parens("NOTIFY", self.state_type.clone())
                    } else if self.state_type.starts_with("ALERTHANDLER (") {
                        parens("EXIT_CODE", exit_code(self.state))
                    } else {
                        self.state_type.clone()
                    }
                }
                "PASSIVE HOST CHECK" => parens("PASSIVE", host_state(self.state)),
                "PASSIVE SERVICE CHECK" => parens("PASSIVE", svc_state(self.state)),
                _ => String::new(),
            },
        }
    }
}

/// A batch of consecutive log entries of one file.
#[derive(Debug, Default)]
pub struct LogChunk {
    pub entries: Vec<LogEntry>,
}

struct CachedFile {
    ino: (u64, u64),
    since: i64,
    offset: u64,
    next_lineno: i64,
    chunks: Vec<Arc<LogChunk>>,
    entries: usize,
    last_used: u64,
}

#[derive(Default)]
struct Cache {
    files: HashMap<PathBuf, CachedFile>,
    tick: u64,
}

/// Upper bound of cached log entries before archive files are evicted.
const MAX_CACHED_ENTRIES: usize = 2_000_000;

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn first_timestamp(path: &Path) -> Option<i64> {
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let b = line.as_bytes();
    if b.len() >= 12 && b[0] == b'[' && b[11] == b']' {
        line[1..11].parse().ok()
    } else {
        None
    }
}

/// Bring the cache entry of `path` up to date; returns its chunks.
fn refresh(c: &mut Cache, path: &Path) -> Option<Vec<Arc<LogChunk>>> {
    let meta = std::fs::metadata(path).ok()?;
    let ino = (meta.dev(), meta.ino());
    let len = meta.len();
    c.tick += 1;
    let tick = c.tick;
    let stale = match c.files.get(path) {
        Some(f) => f.ino != ino || len < f.offset,
        None => true,
    };
    if stale {
        let since = first_timestamp(path)?;
        c.files.insert(
            path.to_path_buf(),
            CachedFile { ino, since, offset: 0, next_lineno: 1, chunks: Vec::new(), entries: 0, last_used: tick },
        );
    }
    let entry = c.files.get_mut(path)?;
    entry.last_used = tick;
    if len > entry.offset {
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(entry.offset)).ok()?;
        let mut buf = Vec::with_capacity((len - entry.offset) as usize);
        f.take(len - entry.offset).read_to_end(&mut buf).ok()?;
        // only consume complete lines
        let complete = buf.iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
        let mut chunk = LogChunk::default();
        for raw in buf[..complete].split(|&b| b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            let lineno = entry.next_lineno;
            entry.next_lineno += 1;
            let line = String::from_utf8_lossy(raw);
            if let Some(e) = LogEntry::parse(lineno, &line) {
                chunk.entries.push(e);
            }
        }
        entry.offset += complete as u64;
        if !chunk.entries.is_empty() {
            entry.entries += chunk.entries.len();
            entry.chunks.push(Arc::new(chunk));
        }
    }
    Some(entry.chunks.clone())
}

fn evict(c: &mut Cache, keep: &Path) {
    let mut total: usize = c.files.values().map(|f| f.entries).sum();
    while total > MAX_CACHED_ENTRIES {
        let victim = c
            .files
            .iter()
            .filter(|(p, _)| p.as_path() != keep)
            .min_by_key(|(_, f)| f.last_used)
            .map(|(p, _)| p.clone());
        match victim.and_then(|v| c.files.remove(&v)) {
            Some(f) => total -= f.entries,
            None => break,
        }
    }
}

/// Load all log chunks that may contain entries in `[since, until]`, oldest first.
pub fn load(paths: &Paths, (since, until): (i64, i64)) -> Vec<Arc<LogChunk>> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &paths.log_archive_path {
        if let Ok(rd) = std::fs::read_dir(dir) {
            files.extend(
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "log")),
            );
        }
    }
    let current = paths.log_file.clone();
    if let Some(cur) = &current {
        if cur.is_file() {
            files.push(cur.clone());
        }
    }

    let Ok(mut c) = cache().lock() else { return Vec::new() };
    // determine the first timestamp of every file (cached for known files)
    let mut with_since: Vec<(i64, PathBuf)> = files
        .into_iter()
        .filter_map(|p| {
            let since = match c.files.get(&p) {
                Some(f) => Some(f.since),
                None => first_timestamp(&p),
            }?;
            Some((since, p))
        })
        .collect();
    with_since.sort();

    let mut out = Vec::new();
    for i in 0..with_since.len() {
        let (file_since, path) = &with_since[i];
        let next_since = with_since.get(i + 1).map(|x| x.0).unwrap_or(i64::MAX);
        if *file_since > until || next_since < since {
            continue;
        }
        if let Some(chunks) = refresh(&mut c, path) {
            out.extend(chunks);
        }
    }
    if let Some(cur) = &current {
        evict(&mut c, cur);
    }
    out
}

macro_rules! own {
    ($x:ident => $e:expr) => {
        Some(getter(move |_c, r| match r {
            Row::Log($x) => $e,
            _ => Value::Null,
        }))
    };
}

pub fn column(name: &str) -> Option<Getter> {
    match name {
        "time" => own!(e => Value::Int(e.time)),
        "lineno" => own!(e => Value::Int(e.lineno)),
        "class" => own!(e => Value::Int(e.class)),
        "message" => own!(e => Value::str(&e.message)),
        "type" => own!(e => Value::str(&e.type_)),
        "options" => own!(e => Value::str(&e.options)),
        "state" => own!(e => Value::Int(e.state)),
        "state_type" => own!(e => Value::str(&e.state_type)),
        "state_info" => own!(e => Value::Str(e.state_info())),
        "attempt" => own!(e => Value::Int(e.attempt)),
        "host_name" => own!(e => Value::str(&e.host_name)),
        "service_description" => own!(e => Value::str(&e.service_description)),
        "contact_name" => own!(e => Value::str(&e.contact_name)),
        "command_name" => own!(e => Value::str(&e.command_name)),
        "plugin_output" => own!(e => Value::str(&e.plugin_output)),
        "long_plugin_output" => own!(e => Value::str(&e.long_plugin_output)),
        "comment" => own!(e => Value::str(&e.comment)),
        "current_command_name" => Some(getter(|c, r| match r {
            Row::Log(e) => {
                Value::Str(c.world.objects.commands.get(&e.command_name).map(|x| x.name.clone()).unwrap_or_default())
            }
            _ => Value::Null,
        })),
        "current_command_line" => Some(getter(|c, r| match r {
            Row::Log(e) => {
                Value::Str(c.world.objects.commands.get(&e.command_name).map(|x| x.line.clone()).unwrap_or_default())
            }
            _ => Value::Null,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_service_alert() {
        let e = LogEntry::parse(
            7,
            "[1795004087] SERVICE ALERT: web01;Check_MK Discovery;WARNING;HARD;1;Services unmonitored: 7",
        )
        .unwrap();
        assert_eq!(e.class, class::ALERT);
        assert_eq!(e.type_, "SERVICE ALERT");
        assert_eq!(e.state, 1);
        assert_eq!(e.state_type, "HARD");
        assert_eq!(e.host_name, "web01");
        assert_eq!(e.service_description, "Check_MK Discovery");
        assert_eq!(e.plugin_output, "Services unmonitored: 7");
        assert_eq!(e.state_info(), "HARD (WARNING)");
        assert_eq!(e.options, "web01;Check_MK Discovery;WARNING;HARD;1;Services unmonitored: 7");
    }

    #[test]
    fn parses_notification_and_commands() {
        let e = LogEntry::parse(1, "[1795004054] SERVICE NOTIFICATION: check-mk-notify;localhost;Check_MK Discovery;WARNING;check-mk-notify;out").unwrap();
        assert_eq!(e.class, class::NOTIFICATION);
        assert_eq!(e.contact_name, "check-mk-notify");
        assert_eq!(e.state, 1);
        assert_eq!(e.state_info(), "NOTIFY (WARNING)");

        let e = LogEntry::parse(2, "[1795004054] EXTERNAL COMMAND: ADD_SVC_COMMENT;h;s;1;me;hi").unwrap();
        assert_eq!(e.class, class::EXT_COMMAND);
        assert_eq!((e.host_name.as_str(), e.service_description.as_str()), ("h", "s"));

        let e = LogEntry::parse(3, "[1795004054] EXTERNAL COMMAND: SCHEDULE_HOST_DOWNTIME;h;1;2").unwrap();
        assert_eq!((e.host_name.as_str(), e.service_description.as_str()), ("h", ""));

        let e = LogEntry::parse(4, "[1795004054] Nagios 3.5.1 starting... (PID=1)").unwrap();
        assert_eq!(e.class, class::PROGRAM);
        assert!(LogEntry::parse(5, "garbage").is_none());
    }

    #[test]
    fn loads_files_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        std::fs::create_dir(&archive).unwrap();
        std::fs::write(
            archive.join("nagios-01-01-2026-00.log"),
            "[1700000100] LOG VERSION: 2.0\n[1700000150] HOST ALERT: a;DOWN;HARD;1;x\n",
        )
        .unwrap();
        let cur = dir.path().join("nagios.log");
        std::fs::write(&cur, "[1700000200] LOG VERSION: 2.0\n").unwrap();
        let paths = Paths { log_file: Some(cur.clone()), log_archive_path: Some(archive), ..Default::default() };

        let all: usize = load(&paths, (0, i64::MAX)).iter().map(|c| c.entries.len()).sum();
        assert_eq!(all, 3);
        // only the current file overlaps
        let recent: usize = load(&paths, (1700000201, i64::MAX)).iter().map(|c| c.entries.len()).sum();
        assert_eq!(recent, 1);

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&cur).unwrap();
        writeln!(f, "[1700000210] SERVICE ALERT: a;b;OK;HARD;1;fine").unwrap();
        write!(f, "[1700000220] partial line").unwrap();
        let recent: Vec<_> = load(&paths, (1700000201, i64::MAX));
        let entries: Vec<&LogEntry> = recent.iter().flat_map(|c| c.entries.iter()).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].lineno, 2);
    }
}
