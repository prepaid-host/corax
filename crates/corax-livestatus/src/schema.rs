//! The Livestatus table schema.
//!
//! The schema is the one exposed by the original Livestatus (Checkmk 2.5, Nagios core),
//! embedded at compile time. corax exposes exactly these tables and columns so that
//! clients written against the original Livestatus keep working; columns for which
//! corax has no data return a type-correct default value.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::value::ColumnType;

const SCHEMA_TSV: &str = include_str!("schema.tsv");

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub table: &'static str,
    pub name: &'static str,
    pub ty: ColumnType,
    pub description: &'static str,
}

struct Schema {
    all: Vec<ColumnDef>,
    /// table → index range into `all`
    tables: HashMap<&'static str, (usize, usize)>,
    /// (table, column) → index into `all`
    by_name: HashMap<(&'static str, &'static str), usize>,
}

fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let mut all: Vec<ColumnDef> = SCHEMA_TSV
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(4, '\t');
                let table = parts.next()?;
                let name = parts.next()?;
                let ty = ColumnType::parse(parts.next()?)?;
                let description = parts.next().unwrap_or("");
                Some(ColumnDef { table, name, ty, description })
            })
            .collect();
        // keep tables contiguous, preserving the column order within a table
        all.sort_by_key(|c| c.table);
        let mut tables = HashMap::new();
        let mut by_name = HashMap::new();
        let mut start = 0;
        for i in 0..=all.len() {
            if i == all.len() || all[i].table != all[start].table {
                if i > start {
                    tables.insert(all[start].table, (start, i));
                }
                start = i;
            }
            if i < all.len() {
                by_name.insert((all[i].table, all[i].name), i);
            }
        }
        Schema { all, tables, by_name }
    })
}

/// All columns of all tables.
pub fn all_columns() -> &'static [ColumnDef] {
    &schema().all
}

/// Columns of `table` in schema order, or `None` for unknown tables.
pub fn table_columns(table: &str) -> Option<&'static [ColumnDef]> {
    let s = schema();
    s.tables.get(table).map(|&(a, b)| &s.all[a..b])
}

/// Look up a column by its exact name.
pub fn find(table: &str, name: &str) -> Option<&'static ColumnDef> {
    let s = schema();
    let (&t, _) = s.tables.get_key_value(table)?;
    let idx = *s.by_name.get(&(t, name))?;
    Some(&s.all[idx])
}

/// The prefix that may be prepended to column names of a table (`host_name` in `hosts`).
pub fn name_prefix(table: &str) -> &'static str {
    match table {
        "hosts" | "hostsbygroup" => "host_",
        "services" | "servicesbygroup" | "servicesbyhostgroup" => "service_",
        "hostgroups" => "hostgroup_",
        "servicegroups" => "servicegroup_",
        "contacts" => "contact_",
        "contactgroups" => "contactgroup_",
        "commands" => "command_",
        "timeperiods" => "timeperiod_",
        "comments" => "comment_",
        "downtimes" => "downtime_",
        "log" => "log_",
        "status" => "status_",
        "columns" => "column_",
        "crashreports" => "crashreport_",
        "labels" => "label_",
        "statehist" => "statehist_",
        "eventconsoleevents" | "eventconsolehistory" => "event_",
        _ => "",
    }
}

/// Resolve a requested column name for a table like the original Livestatus: strip
/// any number of table prefixes, try an exact match, then the prefixed name. The
/// `log` and `statehist` tables additionally accept `current_` joins without the
/// prefix (`host_name` → `current_host_name` if not an own column).
///
/// On failure returns the (stripped) name for the error message.
pub fn resolve(table: &str, name: &str) -> Result<&'static ColumnDef, String> {
    let lookup = |name: &str| -> Result<&'static ColumnDef, String> {
        let prefix = name_prefix(table);
        let mut n = name;
        if !prefix.is_empty() {
            while let Some(rest) = n.strip_prefix(prefix) {
                n = rest;
            }
        }
        find(table, n).or_else(|| find(table, &format!("{prefix}{n}"))).ok_or_else(|| n.to_string())
    };
    match lookup(name) {
        Ok(def) => Ok(def),
        Err(stripped) if matches!(table, "log" | "statehist") => {
            lookup(&format!("current_{name}")).map_err(|_| stripped)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_loaded() {
        assert_eq!(table_columns("hosts").map(|c| c.len()), Some(134));
        assert_eq!(table_columns("services").map(|c| c.len()), Some(244));
        assert_eq!(find("hosts", "state").map(|c| c.ty), Some(ColumnType::Int));
        assert!(table_columns("nope").is_none());
    }

    #[test]
    fn prefix_resolution() {
        let r = |t, n| resolve(t, n).map(|c| c.name);
        assert_eq!(r("hosts", "host_name"), Ok("name"));
        assert_eq!(r("services", "host_name"), Ok("host_name"));
        assert_eq!(r("services", "service_description"), Ok("description"));
        assert_eq!(r("services", "service_service_description"), Ok("description"));
        assert_eq!(r("hostgroups", "hostgroup_num_hosts"), Ok("num_hosts"));
        assert_eq!(r("log", "host_state"), Ok("current_host_state"));
        assert_eq!(r("log", "log_time"), Ok("time"));
        assert_eq!(r("hosts", "host_bogus"), Err("bogus".to_string()));
    }
}
