//! Filter expressions (`Filter:`, `Stats:`, `WaitCondition:` headers).

use regex::{Regex, RegexBuilder};

use crate::tables::{Column, Ctx, Row};
use crate::value::{ColumnType, Value};

/// Relational operator of a filter line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Match,
    NoMatch,
    EqIcase,
    NeIcase,
    MatchIcase,
    NoMatchIcase,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "=" => Op::Eq,
            "!=" => Op::Ne,
            "~" => Op::Match,
            "!~" => Op::NoMatch,
            "=~" => Op::EqIcase,
            "!=~" => Op::NeIcase,
            "~~" => Op::MatchIcase,
            "!~~" => Op::NoMatchIcase,
            "<" => Op::Lt,
            ">" => Op::Gt,
            "<=" => Op::Le,
            ">=" => Op::Ge,
            _ => return None,
        })
    }

    fn uses_regex(self) -> bool {
        matches!(self, Op::Match | Op::NoMatch | Op::MatchIcase | Op::NoMatchIcase)
    }

    fn icase(self) -> bool {
        matches!(self, Op::EqIcase | Op::NeIcase | Op::MatchIcase | Op::NoMatchIcase)
    }
}

/// A compiled filter tree.
#[derive(Clone)]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
    Column(Box<ColumnFilter>),
}

/// Maximum nesting of `And`/`Or`/`Negate` (recursion during evaluation is bounded by it).
pub const MAX_FILTER_DEPTH: usize = 64;

impl Filter {
    /// Nesting depth. Only called on filters whose children are already bounded.
    pub fn depth(&self) -> usize {
        match self {
            Filter::Column(_) => 1,
            Filter::Not(f) => 1 + f.depth(),
            Filter::And(v) | Filter::Or(v) => 1 + v.iter().map(Filter::depth).max().unwrap_or(0),
        }
    }
}

#[derive(Clone)]
pub struct ColumnFilter {
    pub column: Column,
    pub op: Op,
    /// Reference value (for dict columns: the value part after the key).
    pub value: String,
    key: Option<String>,
    regex: Option<Regex>,
    num: f64,
    int: i64,
}

fn build_regex(pattern: &str, icase: bool) -> Result<Regex, String> {
    RegexBuilder::new(pattern).case_insensitive(icase).size_limit(1 << 20).build().map_err(|e| {
        // keep the message on one line, like Livestatus/RE2
        e.to_string().lines().last().unwrap_or("invalid regular expression").trim().to_string()
    })
}

/// Parse a dict filter value `key value` (both parts optionally single-quoted,
/// `''` escapes a quote) like Livestatus' `DictStrValueFilter`.
fn split_dict_value(s: &str) -> (String, String) {
    fn parse_quoted(s: &str) -> (String, &str) {
        let mut out = String::new();
        let mut rest = s;
        loop {
            match rest.find('\'') {
                None => return (out + rest, ""),
                Some(p) => {
                    out.push_str(&rest[..p]);
                    rest = &rest[p + 1..];
                    if let Some(r) = rest.strip_prefix('\'') {
                        out.push('\'');
                        rest = r;
                    } else {
                        return (out, rest);
                    }
                }
            }
        }
    }
    let s = s.trim_start();
    let (key, rest) = if let Some(q) = s.strip_prefix('\'') {
        parse_quoted(q)
    } else {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        (s[..end].to_string(), &s[end..])
    };
    let rest = rest.trim_start();
    let value = match rest.strip_prefix('\'') {
        Some(q) => parse_quoted(q).0,
        None => rest.to_string(),
    };
    (key, value)
}

impl ColumnFilter {
    pub fn new(column: Column, op: Op, raw_value: &str) -> Result<Self, String> {
        let (key, value) = match column.ty {
            ColumnType::Dict | ColumnType::DictDouble => {
                let (k, v) = split_dict_value(raw_value);
                (Some(k), v)
            }
            _ => (None, raw_value.to_string()),
        };
        let regex = match column.ty {
            ColumnType::List => match op {
                // element-wise exact / case-insensitive matching is done without a regex
                _ if op.uses_regex() => Some(build_regex(&value, op.icase())?),
                _ => None,
            },
            ColumnType::Int | ColumnType::Time | ColumnType::Float => None,
            _ if op.uses_regex() => Some(build_regex(&value, op.icase())?),
            _ => None,
        };
        let num = value.trim().parse::<f64>().unwrap_or(0.0);
        let int = parse_int_prefix(&value);
        Ok(ColumnFilter { column, op, value, key, regex, num, int })
    }

    fn accepts(&self, ctx: &Ctx<'_>, row: &Row<'_>) -> bool {
        let v = (self.column.get)(ctx, row);
        match self.column.ty {
            ColumnType::Int | ColumnType::Time => compare_ord(self.op, v.as_i64().cmp(&self.int)),
            ColumnType::Float => match v.as_f64().partial_cmp(&self.num) {
                Some(o) => compare_ord(self.op, o),
                None => matches!(self.op, Op::Ne),
            },
            ColumnType::List => self.accepts_list(&v),
            ColumnType::Dict => {
                let text =
                    dict_lookup(&v, self.key.as_deref().unwrap_or_default()).map(|x| x.as_text()).unwrap_or_default();
                self.accepts_str(&text)
            }
            ColumnType::DictDouble => {
                let num = dict_lookup(&v, self.key.as_deref().unwrap_or_default()).map(|x| x.as_f64()).unwrap_or(0.0);
                match num.partial_cmp(&self.value.trim().parse::<f64>().unwrap_or(0.0)) {
                    Some(o) => compare_ord(self.op, o),
                    None => false,
                }
            }
            ColumnType::String | ColumnType::Blob => {
                // a null (unknown) value behaves like an empty string
                self.accepts_str(&v.as_text())
            }
        }
    }

    fn accepts_str(&self, s: &str) -> bool {
        let r = &self.value;
        match self.op {
            Op::Eq => s == r,
            Op::Ne => s != r,
            Op::EqIcase => s.to_lowercase() == r.to_lowercase(),
            Op::NeIcase => s.to_lowercase() != r.to_lowercase(),
            Op::Match | Op::MatchIcase => self.regex.as_ref().is_some_and(|re| re.is_match(s)),
            Op::NoMatch | Op::NoMatchIcase => !self.regex.as_ref().is_some_and(|re| re.is_match(s)),
            Op::Lt => s < r.as_str(),
            Op::Gt => s > r.as_str(),
            Op::Le => s <= r.as_str(),
            Op::Ge => s >= r.as_str(),
        }
    }

    fn accepts_list(&self, v: &Value) -> bool {
        let empty = Vec::new();
        let items = match v {
            Value::List(l) => l,
            _ => &empty,
        };
        let r = &self.value;
        let any_eq = || items.iter().any(|i| i.as_text() == *r);
        let any_eq_ci = || {
            let lr = r.to_lowercase();
            items.iter().any(|i| i.as_text().to_lowercase() == lr)
        };
        let any_re = || self.regex.as_ref().is_some_and(|re| items.iter().any(|i| re.is_match(&i.as_text())));
        match self.op {
            // equality is only defined for emptiness checks
            Op::Eq => r.is_empty() && items.is_empty(),
            Op::Ne => r.is_empty() && !items.is_empty(),
            Op::Ge => any_eq(),
            Op::Lt => !any_eq(),
            Op::Le => any_eq_ci(),
            Op::Gt => !any_eq_ci(),
            Op::Match | Op::MatchIcase => any_re(),
            Op::NoMatch | Op::NoMatchIcase => !any_re(),
            Op::EqIcase | Op::NeIcase => false,
        }
    }
}

/// `std::from_chars`-like: parse a leading (optionally signed) integer, else 0.
fn parse_int_prefix(s: &str) -> i64 {
    let s = s.trim();
    let end = s
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    s[..end].parse().unwrap_or(0)
}

fn dict_lookup<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
    match v {
        Value::Dict(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

fn compare_ord(op: Op, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        Op::Eq | Op::EqIcase => o == Equal,
        Op::Ne | Op::NeIcase => o != Equal,
        Op::Lt => o == Less,
        Op::Gt => o == Greater,
        Op::Le => o != Greater,
        Op::Ge => o != Less,
        Op::Match | Op::NoMatch | Op::MatchIcase | Op::NoMatchIcase => false,
    }
}

impl Filter {
    pub fn accepts(&self, ctx: &Ctx<'_>, row: &Row<'_>) -> bool {
        match self {
            Filter::And(fs) => fs.iter().all(|f| f.accepts(ctx, row)),
            Filter::Or(fs) => fs.iter().any(|f| f.accepts(ctx, row)),
            Filter::Not(f) => !f.accepts(ctx, row),
            Filter::Column(cf) => cf.accepts(ctx, row),
        }
    }

    /// Bounds on `column` implied by top-level conjunctive filters, as `(min, max)`.
    /// Used to restrict which log files are read.
    pub fn int_bounds(&self, column: &str) -> (i64, i64) {
        let mut lo = i64::MIN;
        let mut hi = i64::MAX;
        self.collect_bounds(column, &mut lo, &mut hi);
        (lo, hi)
    }

    fn collect_bounds(&self, column: &str, lo: &mut i64, hi: &mut i64) {
        match self {
            Filter::And(fs) => fs.iter().for_each(|f| f.collect_bounds(column, lo, hi)),
            Filter::Column(cf) if cf.column.canonical == column => match cf.op {
                Op::Ge => *lo = (*lo).max(cf.int),
                Op::Gt => *lo = (*lo).max(cf.int.saturating_add(1)),
                Op::Le => *hi = (*hi).min(cf.int),
                Op::Lt => *hi = (*hi).min(cf.int.saturating_sub(1)),
                Op::Eq => {
                    *lo = (*lo).max(cf.int);
                    *hi = (*hi).min(cf.int);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::getter;
    use std::sync::Arc;

    fn col(ty: ColumnType, v: Value) -> Column {
        let v = Arc::new(v);
        Column { name: "c".into(), canonical: "c", ty, get: getter(move |_, _| (*v).clone()), rrd: None }
    }

    fn check(ty: ColumnType, v: Value, op: &str, reference: &str) -> bool {
        let world = test_world();
        let paths = corax_core::Paths::default();
        let ctx = Ctx { world: &world, paths: &paths, now: 0, auth: crate::tables::Auth::All };
        let f = ColumnFilter::new(col(ty, v), Op::parse(op).unwrap(), reference).unwrap();
        f.accepts(&ctx, &crate::tables::Row::Status)
    }

    fn test_world() -> corax_core::World {
        let cfg = corax_config::LoadedConfig {
            main: Default::default(),
            resources: Default::default(),
            objects: Default::default(),
            warnings: vec![],
        };
        corax_core::World::new(&cfg, 0)
    }

    #[test]
    fn int_ops() {
        assert!(check(ColumnType::Int, Value::Int(2), ">=", "1"));
        assert!(!check(ColumnType::Int, Value::Int(0), ">", "0"));
        assert!(check(ColumnType::Int, Value::Int(0), "=", "x"));
        assert!(check(ColumnType::Time, Value::Int(5), "!=", "4"));
    }

    #[test]
    fn string_ops() {
        assert!(check(ColumnType::String, Value::str("Memory"), "=", "Memory"));
        assert!(check(ColumnType::String, Value::str("Memory"), "=~", "memory"));
        assert!(check(ColumnType::String, Value::str("CPU load"), "~", "load$"));
        assert!(check(ColumnType::String, Value::str("CPU load"), "~~", "^cpu"));
        assert!(check(ColumnType::String, Value::str("CPU load"), "!~", "^cpu"));
        assert!(check(ColumnType::String, Value::str("b"), ">", "a"));
    }

    #[test]
    fn list_ops() {
        let l = || Value::str_list(["check_mk", "Linux"]);
        assert!(check(ColumnType::List, l(), ">=", "check_mk"));
        assert!(!check(ColumnType::List, l(), ">=", "linux"));
        assert!(check(ColumnType::List, l(), "<=", "linux"));
        assert!(check(ColumnType::List, l(), "<", "windows"));
        assert!(check(ColumnType::List, l(), "~", "^Lin"));
        assert!(check(ColumnType::List, Value::List(vec![]), "=", ""));
        assert!(!check(ColumnType::List, l(), "=", ""));
        assert!(check(ColumnType::List, l(), "!=", ""));
    }

    #[test]
    fn dict_ops() {
        let d = || Value::str_dict([("cmk/os_family", "linux"), ("my label", "a b")]);
        assert!(check(ColumnType::Dict, d(), "=", "cmk/os_family linux"));
        assert!(!check(ColumnType::Dict, d(), "=", "cmk/os_family windows"));
        assert!(check(ColumnType::Dict, d(), "=", "'my label' 'a b'"));
        assert!(check(ColumnType::Dict, d(), "~", "cmk/os_family ^lin"));
        assert!(check(ColumnType::Dict, d(), "=", "missing "));
    }

    #[test]
    fn invalid_regex() {
        assert!(ColumnFilter::new(col(ColumnType::String, Value::Null), Op::Match, "[").is_err());
    }

    #[test]
    fn int_prefix() {
        assert_eq!(parse_int_prefix(" 42abc"), 42);
        assert_eq!(parse_int_prefix("-3"), -3);
        assert_eq!(parse_int_prefix("x"), 0);
    }
}
