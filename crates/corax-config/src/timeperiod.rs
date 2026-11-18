//! Time period definitions and evaluation.
//!
//! Supported: weekday ranges (`monday 00:00-09:00,17:00-24:00`), calendar date
//! exceptions (`2026-12-24 00:00-24:00`, `2026-12-24 - 2026-12-26 00:00-24:00`),
//! `exclude` lists. Other exception formats are parsed leniently and ignored with a
//! warning by the resolver.

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use std::collections::HashMap;

/// Seconds-since-midnight interval `[start, end)`.
pub type Range = (u32, u32);

#[derive(Debug, Clone, Default)]
pub struct Timeperiod {
    pub name: String,
    pub alias: String,
    /// Index 0 = sunday … 6 = saturday (same as `tm_wday`).
    pub weekdays: [Vec<Range>; 7],
    /// Date exceptions, these take precedence over weekdays for that day.
    pub dates: Vec<(NaiveDate, NaiveDate, Vec<Range>)>,
    pub exclude: Vec<String>,
    /// Lines we did not understand (reported as warnings).
    pub unsupported: Vec<String>,
}

const WEEKDAYS: [&str; 7] = ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"];

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 24 && m < 60 && h * 3600 + m * 60 <= 86400).then_some(h * 3600 + m * 60)
}

/// Parse `00:00-09:00,17:00-24:00`.
pub fn parse_ranges(s: &str) -> Option<Vec<Range>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-')?;
        let (a, b) = (parse_hhmm(a)?, parse_hhmm(b)?);
        if a < b {
            out.push((a, b));
        }
    }
    Some(out)
}

impl Timeperiod {
    /// Add one attribute line (`key value`).
    pub fn add_attr(&mut self, key: &str, value: &str) {
        match key {
            "timeperiod_name" => self.name = value.to_string(),
            "alias" => self.alias = value.to_string(),
            "exclude" => self.exclude = value.split(',').map(|s| s.trim().to_string()).collect(),
            "name" | "use" | "register" => {}
            _ => {
                if let Some(idx) = WEEKDAYS.iter().position(|d| *d == key) {
                    match parse_ranges(value) {
                        Some(r) => self.weekdays[idx] = r,
                        None => self.unsupported.push(format!("{key} {value}")),
                    }
                } else if !self.parse_date_exception(key, value) {
                    self.unsupported.push(format!("{key} {value}"));
                }
            }
        }
    }

    /// Handles `YYYY-MM-DD` keys, optionally followed by `- YYYY-MM-DD` inside value.
    fn parse_date_exception(&mut self, key: &str, value: &str) -> bool {
        let Ok(start) = NaiveDate::parse_from_str(key, "%Y-%m-%d") else {
            return false;
        };
        let mut end = start;
        let mut rest = value.trim();
        if let Some(r) = rest.strip_prefix('-') {
            let r = r.trim_start();
            let (date, tail) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
            match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
                Ok(d) => end = d,
                Err(_) => return false,
            }
            rest = tail.trim();
        }
        match parse_ranges(rest) {
            Some(r) => {
                self.dates.push((start, end, r));
                true
            }
            None => false,
        }
    }

    fn ranges_for(&self, date: NaiveDate) -> &[Range] {
        for (start, end, ranges) in &self.dates {
            if *start <= date && date <= *end {
                return ranges;
            }
        }
        &self.weekdays[date.weekday().num_days_from_sunday() as usize]
    }

    /// Whether `t` lies inside this period, ignoring `exclude`.
    fn contains_local(&self, t: &DateTime<Local>) -> bool {
        let secs = t.num_seconds_from_midnight();
        self.ranges_for(t.date_naive()).iter().any(|(a, b)| *a <= secs && secs < *b)
    }
}

/// Evaluate whether `name` is active at unix time `t`. Unknown or empty names are
/// treated as "always" (Nagios semantics for an unset check_period is "24x7").
pub fn is_active(periods: &HashMap<String, Timeperiod>, name: Option<&str>, t: i64) -> bool {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return true;
    };
    let Some(local) = Local.timestamp_opt(t, 0).single() else {
        return true;
    };
    is_active_rec(periods, name, &local, 0)
}

fn is_active_rec(periods: &HashMap<String, Timeperiod>, name: &str, t: &DateTime<Local>, depth: u8) -> bool {
    let Some(tp) = periods.get(name) else {
        return true;
    };
    if depth > 8 {
        return false;
    }
    tp.contains_local(t)
        && !tp.exclude.iter().any(|ex| periods.contains_key(ex) && is_active_rec(periods, ex, t, depth + 1))
}

/// Find the next time ≥ `t` at which `name` is active (minute resolution, max 31 days).
pub fn next_active(periods: &HashMap<String, Timeperiod>, name: Option<&str>, t: i64) -> Option<i64> {
    if is_active(periods, name, t) {
        return Some(t);
    }
    let mut cur = t - t.rem_euclid(60) + 60;
    let limit = t + 31 * 86400;
    while cur < limit {
        if is_active(periods, name, cur) {
            return Some(cur);
        }
        cur += 60;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(lines: &[(&str, &str)]) -> HashMap<String, Timeperiod> {
        let mut t = Timeperiod::default();
        for (k, v) in lines {
            t.add_attr(k, v);
        }
        let mut m = HashMap::new();
        m.insert(t.name.clone(), t);
        m
    }

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).single().unwrap().timestamp()
    }

    #[test]
    fn always() {
        let all: Vec<(&str, &str)> =
            std::iter::once(("timeperiod_name", "24X7")).chain(WEEKDAYS.iter().map(|d| (*d, "00:00-24:00"))).collect();
        let p = tp(&all);
        assert!(is_active(&p, Some("24X7"), local(2026, 11, 18, 23, 59)));
    }

    #[test]
    fn workhours_and_exception() {
        let p = tp(&[("timeperiod_name", "work"), ("wednesday", "08:00-17:00"), ("2026-11-18", "10:00-11:00")]);
        // 2026-11-18 is a wednesday, exception overrides
        assert!(!is_active(&p, Some("work"), local(2026, 11, 18, 9, 0)));
        assert!(is_active(&p, Some("work"), local(2026, 11, 18, 10, 30)));
        assert!(is_active(&p, Some("work"), local(2026, 11, 25, 9, 0)));
        assert_eq!(next_active(&p, Some("work"), local(2026, 11, 18, 9, 0)), Some(local(2026, 11, 18, 10, 0)));
    }

    #[test]
    fn unknown_period_is_always() {
        assert!(is_active(&HashMap::new(), Some("nope"), 0));
        assert!(is_active(&HashMap::new(), None, 0));
    }
}
