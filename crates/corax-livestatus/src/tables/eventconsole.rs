//! The `eventconsole*` tables: events, their history, rule hits and the status of the
//! Checkmk Event Console.
//!
//! The data lives in the Event Console daemon (mkeventd). A query is forwarded to
//! its status socket, reduced to the columns it uses and to the filters mkeventd can
//! evaluate itself; the answer is joined with the hosts and filtered here.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use corax_core::{Host, HostId, Paths, World};

use super::{getter, Auth, Ctx, Getter, Row};
use crate::filter::{Filter, Op};
use crate::query::{Query, StatsColumn};
use crate::value::{ColumnType, Value};

/// One row of an answer from mkeventd: column name -> raw value.
#[derive(Debug, Default)]
pub struct EcRow {
    values: HashMap<String, String>,
}

/// "No value" in list columns of mkeventd.
const NONE: &str = "\u{2}";

impl EcRow {
    fn get(&self, name: &str) -> &str {
        self.values.get(name).map(String::as_str).unwrap_or_default()
    }

    /// Like C's `atof`/`atol`: the leading number, 0 if there is none.
    fn number(&self, name: &str) -> f64 {
        let s = self.get(name).trim_start();
        let end = s
            .char_indices()
            .take_while(|&(i, c)| c.is_ascii_digit() || c == '.' || ((c == '-' || c == '+') && i == 0))
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        s[..end].parse().unwrap_or(0.0)
    }

    /// Lists are sent as `\x01a\x01b`, a missing list as `\x02`.
    fn list(&self, name: &str) -> Vec<&str> {
        let s = self.get(name);
        if s.is_empty() || s == NONE {
            Vec::new()
        } else {
            s[1..].split('\u{1}').collect()
        }
    }

    /// The host of the event, see [`Designations`].
    pub fn host_designation(&self) -> &str {
        self.get("event_host")
    }
}

/// Finds the host an event belongs to: by name, alias or address, ignoring case.
pub struct Designations(HashMap<String, HostId>);

impl Designations {
    pub fn new(world: &World) -> Self {
        let mut map = HashMap::with_capacity(world.hosts.len() * 3);
        // later entries win, the host name last
        for h in &world.hosts {
            map.insert(h.cfg.address.to_lowercase(), h.id);
        }
        for h in &world.hosts {
            map.insert(h.cfg.alias.to_lowercase(), h.id);
        }
        for h in &world.hosts {
            map.insert(h.cfg.name.to_lowercase(), h.id);
        }
        Designations(map)
    }

    pub fn find<'a>(&self, world: &'a World, row: &EcRow) -> Option<&'a Host> {
        self.0.get(&row.host_designation().to_lowercase()).map(|id| world.host(*id))
    }
}

/// Whether rows of `table` belong to an event and are subject to authorization.
pub fn has_events(table: &str) -> bool {
    matches!(table, "eventconsoleevents" | "eventconsolehistory")
}

/// Whether the user may see a row. Rows of the status and rule tables are visible to
/// every contact; an event without the fields needed for the decision is not.
pub fn visible(ctx: &Ctx<'_>, row: &EcRow, host: Option<&Host>, is_event: bool) -> bool {
    let contact = match ctx.auth {
        Auth::All => return true,
        Auth::Nobody => return false,
        Auth::Contact(c) => c,
    };
    if !is_event {
        return true;
    }
    if !row.values.contains_key("event_contact_groups_precedence") || !row.values.contains_key("event_contact_groups") {
        return false;
    }
    let groups = row.get("event_contact_groups");
    let via_groups = || {
        row.list("event_contact_groups")
            .iter()
            .any(|name| ctx.world.contactgroups.iter().any(|g| g.cfg.name == *name && g.members.contains(&contact)))
    };
    let via_host = |h: &Host| ctx.host_visible(h);
    match (row.get("event_contact_groups_precedence"), host) {
        ("rule", _) if groups != NONE => via_groups(),
        ("rule", Some(h)) => via_host(h),
        ("host", Some(h)) => via_host(h),
        ("host", None) if groups != NONE => via_groups(),
        ("rule" | "host", _) => true,
        _ => false,
    }
}

/// Columns mkeventd can pre-filter on.
const GREPPING_COLUMNS: &[&str] = &[
    "event_id",
    "event_text",
    "event_comment",
    "event_host",
    "event_contact",
    "event_application",
    "event_rule_id",
    "event_owner",
    "event_ipaddress",
    "event_core_host",
];

/// The request sent to mkeventd for a query.
fn request(q: &Query) -> String {
    let mut columns: BTreeSet<&str> = BTreeSet::new();
    let mut add = |c: &super::Column| {
        if !c.canonical.is_empty() {
            columns.insert(c.canonical);
        }
    };
    q.columns.iter().for_each(&mut add);
    q.filter.for_each_column(&mut add);
    for s in &q.stats {
        match s {
            StatsColumn::Count(f) => f.for_each_column(&mut add),
            StatsColumn::Agg(_, c) => add(c),
        }
    }
    q.order_by.iter().for_each(|(c, _)| add(c));
    if has_events(&q.table) {
        // needed for the host join and for authorization
        columns.extend(["event_host", "event_contact_groups_precedence", "event_contact_groups"]);
    }

    let mut out = format!("GET {}\nOutputFormat: plain\nColumns:", &q.table["eventconsole".len()..]);
    // host columns are joined here
    for c in columns.iter().filter(|c| !c.starts_with("host_")) {
        out.push(' ');
        out.push_str(c);
    }
    let (lo, hi) = q.filter.int_bounds("history_time");
    if lo != i64::MIN {
        out.push_str(&format!("\nFilter: history_time >= {lo}"));
    }
    if hi != i64::MAX {
        out.push_str(&format!("\nFilter: history_time <= {hi}"));
    }
    // top level conditions mkeventd understands; everything is filtered again here
    let conjuncts: &[Filter] = match &q.filter {
        Filter::And(fs) => fs,
        other => std::slice::from_ref(other),
    };
    for f in conjuncts {
        let Filter::Column(cf) = f else { continue };
        let op = match cf.op {
            Op::Eq => "=",
            Op::Match => "~",
            Op::EqIcase => "=~",
            Op::MatchIcase => "~~",
            _ => continue,
        };
        // a value with a line break would end the header line; it is filtered here only
        let single_line = !cf.value.chars().any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'));
        if GREPPING_COLUMNS.contains(&cf.column.canonical) && single_line {
            out.push_str(&format!("\nFilter: {} {op} {}", cf.column.canonical, cf.value));
        }
    }
    out.push_str("\n\n");
    out
}

fn exchange(socket: &Path, request: &str) -> std::io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(request.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(reply)
}

/// The first line names the columns, the values are separated by tabs. The format has
/// no escaping, so a line whose number of fields differs from the header cannot be
/// trusted to have its values in the right columns and is dropped.
fn parse(reply: &[u8]) -> Vec<EcRow> {
    let text = String::from_utf8_lossy(reply);
    let mut lines = text.split('\n').take_while(|l| !l.is_empty());
    let Some(header) = lines.next() else { return Vec::new() };
    let names: Vec<&str> = header.split('\t').collect();
    let mut rows = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != names.len() {
            tracing::warn!(
                "ignoring a malformed line from the Event Console ({} fields, expected {})",
                fields.len(),
                names.len()
            );
            continue;
        }
        rows.push(EcRow { values: names.iter().zip(fields).map(|(n, v)| (n.to_string(), v.to_string())).collect() });
    }
    rows
}

/// Whether the site runs the Event Console (`omd config set MKEVENTD on`).
fn enabled(socket: &Path) -> bool {
    match std::env::var("CONFIG_MKEVENTD") {
        Ok(v) => v == "on",
        Err(_) => socket.exists(),
    }
}

/// Ask mkeventd for the rows of a query. `Err` is the message for a `502` response.
pub fn fetch(paths: &Paths, q: &Query) -> Result<Vec<EcRow>, String> {
    let Some(socket) = paths.mkeventd_socket.as_deref().filter(|s| enabled(s)) else {
        return Ok(Vec::new());
    };
    exchange(socket, &request(q))
        .map(|reply| parse(&reply))
        .map_err(|e| format!("[mkeventd at {}] cannot query the Event Console: {e}", socket.display()))
}

/// The getter of an own column: mkeventd sends text, the schema knows the type.
pub fn column(name: &'static str, ty: ColumnType) -> Getter {
    getter(move |_c, r| {
        let Row::EventConsole(row, _) = r else { return Value::Null };
        match ty {
            ColumnType::Int => Value::Int(row.number(name) as i64),
            ColumnType::Time => Value::Int(row.number(name) as i64),
            ColumnType::Float => Value::Float(row.number(name)),
            ColumnType::List => Value::str_list(row.list(name)),
            _ => Value::str(row.get(name)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_replies() {
        let rows = parse(b"event_id\tevent_text\tevent_contact_groups\tevent_first\n7\thello\t\x01a\x01b\t1700000000.5\n8\t\t\x02\t\n\nignored");
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].number("event_id"), rows[0].get("event_text")), (7.0, "hello"));
        assert_eq!(rows[0].list("event_contact_groups"), ["a", "b"]);
        assert_eq!(rows[0].number("event_first") as i64, 1_700_000_000);
        assert!(rows[1].list("event_contact_groups").is_empty());
        assert_eq!((rows[1].number("event_first"), rows[1].number("missing"), rows[1].get("missing")), (0.0, 0.0, ""));
        assert!(parse(b"").is_empty());
        // too few or too many fields: the values cannot be assigned to columns
        let rows = parse(b"event_id\tevent_text\n1\n2\ta\tb\n3\tok\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("event_text"), "ok");
    }
}
