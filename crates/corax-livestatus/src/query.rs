//! Request parsing (`GET table` + header lines, or `COMMAND`).

use crate::filter::{ColumnFilter, Filter, Op};
use crate::render::{OutputFormat, Separators};
use crate::tables::{self, Column};

/// Livestatus response status codes.
pub mod status {
    pub const OK: u16 = 200;
    pub const BAD_REQUEST: u16 = 400;
    pub const NOT_FOUND: u16 = 404;
}

/// Aggregation function of a `Stats: <op> <column>` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    Sum,
    Min,
    Max,
    Avg,
    Std,
    SumInv,
    AvgInv,
}

impl AggOp {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sum" => Self::Sum,
            "min" => Self::Min,
            "max" => Self::Max,
            "avg" => Self::Avg,
            "std" => Self::Std,
            "suminv" => Self::SumInv,
            "avginv" => Self::AvgInv,
            _ => return None,
        })
    }
}

#[derive(Clone)]
pub enum StatsColumn {
    Count(Filter),
    Agg(AggOp, Column),
}

/// A parsed `GET` request.
pub struct Query {
    pub table: String,
    pub columns: Vec<Column>,
    pub filter: Filter,
    pub stats: Vec<StatsColumn>,
    pub limit: Option<usize>,
    pub output: OutputFormat,
    pub separators: Separators,
    pub column_headers: bool,
    pub fixed16: bool,
    pub keepalive: bool,
    pub auth_user: Option<String>,
    pub wait_object: Option<String>,
    pub wait_condition: Option<Filter>,
    pub wait_timeout_ms: u64,
    pub wait_trigger: Option<String>,
    pub order_by: Vec<(Column, bool)>,
    /// First error encountered while parsing (status code, message).
    pub error: Option<(u16, String)>,
}

/// A request read from a client.
pub enum Request {
    Get(Box<Query>),
    Command(String),
    /// Unparseable request: reply with an error.
    Invalid {
        message: String,
        fixed16: bool,
        keepalive: bool,
    },
}

fn next_arg<'a>(s: &mut &'a str) -> Result<&'a str, String> {
    let t = s.trim_start();
    if t.is_empty() {
        return Err("missing argument".into());
    }
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    let (arg, rest) = t.split_at(end);
    *s = rest;
    Ok(arg)
}

fn next_uint(s: &mut &str) -> Result<usize, String> {
    let arg = next_arg(s)?;
    arg.parse::<usize>().map_err(|_| "expected non-negative integer".to_string())
}

fn no_args(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Ok(())
    } else {
        Err("superfluous argument(s)".into())
    }
}

fn on_off(s: &mut &str) -> Result<bool, String> {
    match next_arg(s)? {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected 'on' or 'off'".into()),
    }
}

/// Reject filter trees nested deeper than [`crate::filter::MAX_FILTER_DEPTH`].
fn bounded(f: Filter) -> Result<Filter, String> {
    if f.depth() > crate::filter::MAX_FILTER_DEPTH {
        return Err(format!("filter nesting exceeds {} levels", crate::filter::MAX_FILTER_DEPTH));
    }
    Ok(f)
}

/// Upper bound for filters, stats and columns per query (protects the core from
/// pathological requests; real GUI queries use a few dozen).
const MAX_QUERY_ITEMS: usize = 10_000;

fn pop_n<T>(stack: &mut Vec<T>, n: usize) -> Result<Vec<T>, String> {
    if stack.len() < n {
        let len = stack.len();
        return Err(format!(
            "cannot combine filters: expecting {n} {}, but only {len} {} on stack",
            if n == 1 { "filter" } else { "filters" },
            if len == 1 { "is" } else { "are" }
        ));
    }
    Ok(stack.split_off(stack.len() - n))
}

fn parse_filter_line(table: &str, line: &str) -> Result<Filter, String> {
    let mut rest = line;
    let name = next_arg(&mut rest)?;
    let op_txt = next_arg(&mut rest)?;
    let op = Op::parse(op_txt).ok_or_else(|| format!("invalid operator '{op_txt}'"))?;
    let column = tables::column(table, name).map_err(|e| format!("table '{table}' has no column '{}'", e.0))?;
    let value = rest.trim_start();
    Ok(Filter::Column(Box::new(ColumnFilter::new(column, op, value)?)))
}

impl Request {
    /// Parse one request (all lines up to, excluding, the empty terminator line).
    pub fn parse(text: &str) -> Request {
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("").trim_end_matches('\r');
        if let Some(cmd) = first.strip_prefix("COMMAND ") {
            return Request::Command(cmd.trim().to_string());
        }
        let Some(rest) = first.strip_prefix("GET") else {
            return Request::Invalid {
                message: format!("Invalid request method '{}'", first.split_whitespace().next().unwrap_or("")),
                fixed16: text.contains("ResponseHeader: fixed16"),
                keepalive: false,
            };
        };
        let table = rest.trim().to_string();
        Request::Get(Box::new(Query::parse(table, lines)))
    }
}

impl Query {
    fn parse<'a>(table: String, lines: impl Iterator<Item = &'a str>) -> Query {
        let mut q = Query {
            columns: Vec::new(),
            filter: Filter::And(Vec::new()),
            stats: Vec::new(),
            limit: None,
            output: OutputFormat::BrokenCsv,
            separators: Separators::default(),
            column_headers: false,
            fixed16: false,
            keepalive: false,
            auth_user: None,
            wait_object: None,
            wait_condition: None,
            wait_timeout_ms: 0,
            wait_trigger: None,
            order_by: Vec::new(),
            error: None,
            table,
        };
        let table_ok = if q.table.is_empty() {
            q.error = Some((status::BAD_REQUEST, "Invalid GET request, missing table name".into()));
            false
        } else if !tables::table_exists(&q.table) {
            q.error = Some((status::NOT_FOUND, format!("Invalid GET request, no such table '{}'", q.table)));
            false
        } else {
            true
        };

        let mut filters: Vec<Filter> = Vec::new();
        let mut waits: Vec<Filter> = Vec::new();
        let mut explicit_headers: Option<bool> = None;
        for raw in lines {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            let (header, value) = match line.split_once(':') {
                Some((h, v)) => (h, v.trim_start()),
                None => (line, ""),
            };
            // headers that are meaningful even for an invalid table
            let result: Result<(), String> = match header {
                "ResponseHeader" => {
                    let mut v = value;
                    match next_arg(&mut v) {
                        Ok("off") => {
                            q.fixed16 = false;
                            Ok(())
                        }
                        Ok("fixed16") => {
                            q.fixed16 = true;
                            Ok(())
                        }
                        Ok(_) => Err("expected 'off' or 'fixed16'".into()),
                        Err(e) => Err(e),
                    }
                }
                "KeepAlive" => {
                    let mut v = value;
                    on_off(&mut v).map(|b| q.keepalive = b)
                }
                "OutputFormat" => {
                    let mut v = value;
                    next_arg(&mut v).and_then(|f| {
                        OutputFormat::parse(f).map(|f| q.output = f).ok_or_else(|| {
                            "missing/invalid output format, use one of 'CSV', 'csv', 'json', 'python', 'python3'".into()
                        })
                    })
                }
                _ if !table_ok => Ok(()),
                // Stop doing work after the first error (only the first one is reported).
                _ if q.error.is_some() => Ok(()),
                _ => q.parse_header(header, value, &mut filters, &mut waits, &mut explicit_headers),
            };
            if let Err(e) = result {
                if q.error.is_none() {
                    q.error = Some((status::BAD_REQUEST, format!("while processing header '{header}': {e}")));
                }
            }
        }
        if !table_ok {
            return q;
        }
        if q.columns.is_empty() && q.stats.is_empty() {
            if let Some(cols) = crate::schema::table_columns(&q.table) {
                q.columns = cols.iter().filter_map(|c| tables::column(&q.table, c.name).ok()).collect();
            }
            q.column_headers = true;
        } else if let Some(h) = explicit_headers {
            q.column_headers = h;
        }
        q.filter = Filter::And(filters);
        if !waits.is_empty() {
            q.wait_condition = Some(Filter::And(waits));
        }
        q
    }

    fn parse_header(
        &mut self,
        header: &str,
        value: &str,
        filters: &mut Vec<Filter>,
        waits: &mut Vec<Filter>,
        explicit_headers: &mut Option<bool>,
    ) -> Result<(), String> {
        let table = self.table.clone();
        let mut v = value;
        if filters.len() + waits.len() + self.stats.len() + self.columns.len() > MAX_QUERY_ITEMS {
            return Err(format!("too many filters, stats or columns (limit {MAX_QUERY_ITEMS})"));
        }
        match header {
            "Filter" => filters.push(parse_filter_line(&table, value)?),
            "And" | "Or" => {
                let n = next_uint(&mut v)?;
                let parts = pop_n(filters, n)?;
                filters.push(bounded(if header == "And" { Filter::And(parts) } else { Filter::Or(parts) })?);
            }
            "Negate" => {
                no_args(value)?;
                let f = pop_n(filters, 1)?.remove(0);
                filters.push(bounded(Filter::Not(Box::new(f)))?);
            }
            "WaitCondition" => waits.push(parse_filter_line(&table, value)?),
            "WaitConditionAnd" | "WaitConditionOr" => {
                let n = next_uint(&mut v)?;
                let parts = pop_n(waits, n)?;
                waits.push(bounded(if header == "WaitConditionAnd" { Filter::And(parts) } else { Filter::Or(parts) })?);
            }
            "WaitConditionNegate" => {
                no_args(value)?;
                let f = pop_n(waits, 1)?.remove(0);
                waits.push(bounded(Filter::Not(Box::new(f)))?);
            }
            "Stats" => {
                let first = next_arg(&mut v)?;
                if let Some(op) = AggOp::parse(first) {
                    let name = next_arg(&mut v)?;
                    let col =
                        tables::column(&table, name).map_err(|e| format!("table '{table}' has no column '{}'", e.0))?;
                    self.stats.push(StatsColumn::Agg(op, col));
                } else {
                    self.stats.push(StatsColumn::Count(parse_filter_line(&table, value)?));
                }
                *explicit_headers = Some(false);
            }
            "StatsAnd" | "StatsOr" => {
                let n = next_uint(&mut v)?;
                let parts = pop_n(&mut self.stats, n)?
                    .into_iter()
                    .map(|s| match s {
                        StatsColumn::Count(f) => Ok(f),
                        StatsColumn::Agg(..) => Err("expected a stats filter, not an aggregation".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.stats.push(StatsColumn::Count(bounded(if header == "StatsAnd" {
                    Filter::And(parts)
                } else {
                    Filter::Or(parts)
                })?));
            }
            "StatsNegate" => {
                no_args(value)?;
                match pop_n(&mut self.stats, 1)?.remove(0) {
                    StatsColumn::Count(f) => self.stats.push(StatsColumn::Count(bounded(Filter::Not(Box::new(f)))?)),
                    StatsColumn::Agg(..) => return Err("expected a stats filter, not an aggregation".into()),
                }
            }
            "Columns" => {
                for name in value.split_whitespace() {
                    let col = match tables::dynamic_column(&table, name) {
                        Some(dynamic) => dynamic?,
                        None => tables::column(&table, name).unwrap_or_else(|_| tables::null_column(name)),
                    };
                    self.columns.push(col);
                }
                *explicit_headers = Some(false);
            }
            "ColumnHeaders" => *explicit_headers = Some(on_off(&mut v)?),
            "Limit" => self.limit = Some(next_uint(&mut v)?),
            "Timelimit" => {
                next_uint(&mut v)?;
            }
            "AuthUser" => self.auth_user = Some(value.trim().to_string()),
            "Separators" => {
                let mut seps = [0u8; 4];
                for s in &mut seps {
                    let n = next_uint(&mut v)?;
                    *s = u8::try_from(n).map_err(|_| "expected non-negative integer".to_string())?;
                }
                self.separators = Separators { dataset: seps[0], field: seps[1], list: seps[2], host_service: seps[3] };
            }
            "WaitObject" => self.wait_object = Some(value.to_string()),
            "WaitTimeout" => self.wait_timeout_ms = next_uint(&mut v)? as u64,
            "WaitTrigger" => self.wait_trigger = Some(next_arg(&mut v)?.to_string()),
            "Localtime" => {
                next_uint(&mut v)?;
            }
            "OrderBy" => {
                let name = next_arg(&mut v)?;
                let desc = match v.trim() {
                    "" | "asc" => false,
                    "desc" => true,
                    _ => return Err("expected 'asc', 'desc' or 'natural'".into()),
                };
                let col =
                    tables::column(&table, name).map_err(|e| format!("table '{table}' has no column '{}'", e.0))?;
                self.order_by.push((col, desc));
            }
            _ => return Err("undefined request header".into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(text: &str) -> Box<Query> {
        match Request::parse(text) {
            Request::Get(q) => q,
            _ => panic!("expected GET"),
        }
    }

    #[test]
    fn basic_query() {
        let q = get("GET services\nColumns: host_name description state\nFilter: state > 0\nFilter: host_name = db01\nOr: 2\nLimit: 5\nOutputFormat: python3\nResponseHeader: fixed16\nKeepAlive: on\nLocaltime: 1795003846\n");
        assert!(q.error.is_none(), "{:?}", q.error);
        assert_eq!(q.columns.len(), 3);
        assert_eq!(q.limit, Some(5));
        assert!(q.fixed16 && q.keepalive && !q.column_headers);
        assert_eq!(q.output, OutputFormat::Python3);
    }

    #[test]
    fn defaults_to_all_columns_with_headers() {
        let q = get("GET contactgroups\n");
        assert_eq!(q.columns.len(), 3);
        assert!(q.column_headers);
    }

    #[test]
    fn errors() {
        let q = get("GET foo\nResponseHeader: fixed16\n");
        assert_eq!(q.error, Some((404, "Invalid GET request, no such table 'foo'".into())));
        assert!(q.fixed16);
        let q = get("GET hosts\nFoo: bar\nResponseHeader: fixed16\n");
        assert_eq!(q.error, Some((400, "while processing header 'Foo': undefined request header".into())));
        let q = get("GET hosts\nColumns: name\nFilter: nope = 1\n");
        assert_eq!(q.error, Some((400, "while processing header 'Filter': table 'hosts' has no column 'nope'".into())));
        let q = get("GET hosts\nAnd: 2\n");
        assert!(q.error.unwrap().1.contains("cannot combine filters"));
    }

    #[test]
    fn unknown_column_in_columns_is_null() {
        let q = get("GET hosts\nColumns: nope name\n");
        assert!(q.error.is_none());
        assert_eq!(q.columns[0].canonical, "");
    }

    #[test]
    fn stats_combinators() {
        let q = get("GET hosts\nStats: state = 0\nStats: state = 1\nStatsAnd: 2\nStatsNegate:\nStats: sum latency\n");
        assert!(q.error.is_none(), "{:?}", q.error);
        assert_eq!(q.stats.len(), 2);
    }

    #[test]
    fn command() {
        assert!(
            matches!(Request::parse("COMMAND [123] DISABLE_NOTIFICATIONS\n"), Request::Command(c) if c == "[123] DISABLE_NOTIFICATIONS")
        );
    }

    #[test]
    fn deeply_nested_filters_are_rejected_not_crashing() {
        let mut req = String::from("GET hosts\nColumns: name\nFilter: name = x\n");
        for _ in 0..100_000 {
            req.push_str("Negate:\n");
        }
        let Request::Get(q) = Request::parse(&req) else { panic!("expected GET") };
        let (status, msg) = q.error.expect("must be rejected");
        assert_eq!(status, 400);
        assert!(msg.contains("nesting"), "{msg}");
    }
}
