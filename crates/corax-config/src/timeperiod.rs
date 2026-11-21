//! Time period definitions and evaluation.
//!
//! Supported: weekday ranges (`monday 00:00-09:00,17:00-24:00`), `exclude` lists and
//! all exception formats of Nagios, each optionally as a range (`- <end>`) and with
//! a skip interval (`/ <days>`):
//!
//! | Example | Meaning |
//! |---|---|
//! | `2026-12-24 00:00-24:00` | calendar date |
//! | `december 25 00:00-24:00` | day of a month, every year |
//! | `day 1 - 15 00:00-24:00` | day of every month, `-1` is the last day |
//! | `monday 3 00:00-24:00` | third monday of every month, `-1` is the last one |
//! | `thursday -1 november 00:00-24:00` | weekday within a month, every year |
//!
//! On a day that matches an exception, its time ranges replace those of the weekday.
//! If several match, the most specific one wins (in the order of the table).

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
    /// Exceptions, most specific kind first. They take precedence over weekdays.
    pub exceptions: Vec<Exception>,
    pub exclude: Vec<String>,
    /// Lines we did not understand (reported as warnings).
    pub unsupported: Vec<String>,
}

/// A day, possibly relative to a month or year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSpec {
    /// `2026-12-24`
    Calendar(NaiveDate),
    /// `december 25`: `day` counts from the end of the month if negative.
    MonthDate { month: u32, day: i32 },
    /// `day 15`
    MonthDay { day: i32 },
    /// `thursday -1 november`: `weekday` 0 is sunday, `nth` counts from the end if negative.
    MonthWeekday { month: u32, weekday: u32, nth: i32 },
    /// `monday 3`
    Weekday { weekday: u32, nth: i32 },
}

/// The time ranges of the days from `start` to `end`, or of every `skip`th of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exception {
    pub start: DateSpec,
    pub end: DateSpec,
    /// 0 or 1: every day.
    pub skip: u32,
    pub ranges: Vec<Range>,
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).and_then(|d| d.pred_opt()).map_or(28, |d| d.day())
}

/// Day `day` of a month; days beyond the end of the month are its last day.
fn day_of_month(year: i32, month: u32, day: i32) -> Option<NaiveDate> {
    let last = days_in_month(year, month) as i32;
    let day = if day < 0 { last + day + 1 } else { day };
    NaiveDate::from_ymd_opt(year, month, day.clamp(1, last) as u32)
}

/// The `nth` given weekday of a month; if there are fewer, the last (or first) one.
fn nth_weekday(year: i32, month: u32, weekday: u32, nth: i32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset = (7 + weekday - first.weekday().num_days_from_sunday()) % 7;
    let count = (days_in_month(year, month) - 1 - offset) / 7 + 1;
    let index = if nth < 0 { count as i32 + nth } else { nth - 1 };
    NaiveDate::from_ymd_opt(year, month, 1 + offset + 7 * index.clamp(0, count as i32 - 1) as u32)
}

impl DateSpec {
    /// More specific kinds win when several exceptions match a day.
    fn rank(&self) -> u8 {
        match self {
            DateSpec::Calendar(_) => 0,
            DateSpec::MonthDate { .. } => 1,
            DateSpec::MonthDay { .. } => 2,
            DateSpec::MonthWeekday { .. } => 3,
            DateSpec::Weekday { .. } => 4,
        }
    }

    /// The date this stands for in the given year and month.
    fn resolve(&self, year: i32, month: u32) -> Option<NaiveDate> {
        match *self {
            DateSpec::Calendar(d) => Some(d),
            DateSpec::MonthDate { month, day } => day_of_month(year, month, day),
            DateSpec::MonthDay { day } => day_of_month(year, month, day),
            DateSpec::MonthWeekday { month, weekday, nth } => nth_weekday(year, month, weekday, nth),
            DateSpec::Weekday { weekday, nth } => nth_weekday(year, month, weekday, nth),
        }
    }

    /// Parse the words of one date. A bare number takes its kind from `start`.
    fn parse(words: &[&str], start: Option<DateSpec>) -> Option<DateSpec> {
        let month = |w: &str| MONTHS.iter().position(|m| *m == w).map(|i| i as u32 + 1);
        let weekday = |w: &str| WEEKDAYS.iter().position(|d| *d == w).map(|i| i as u32);
        let number = |w: &str| w.parse::<i32>().ok().filter(|n| *n != 0 && n.abs() <= 31);
        match (words, start) {
            ([date], _) if date.len() == 10 => NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().map(DateSpec::Calendar),
            (["day", n], _) => Some(DateSpec::MonthDay { day: number(n)? }),
            ([w, n, m], _) => Some(DateSpec::MonthWeekday { month: month(m)?, weekday: weekday(w)?, nth: number(n)? }),
            ([w, n], Some(DateSpec::MonthWeekday { month, .. })) if weekday(w).is_some() => {
                Some(DateSpec::MonthWeekday { month, weekday: weekday(w)?, nth: number(n)? })
            }
            ([w, n], _) if weekday(w).is_some() => Some(DateSpec::Weekday { weekday: weekday(w)?, nth: number(n)? }),
            ([m, n], _) => Some(DateSpec::MonthDate { month: month(m)?, day: number(n)? }),
            ([n], Some(DateSpec::MonthDay { .. })) => Some(DateSpec::MonthDay { day: number(n)? }),
            ([n], Some(DateSpec::MonthDate { month, .. })) => Some(DateSpec::MonthDate { month, day: number(n)? }),
            ([n], Some(DateSpec::Weekday { weekday, .. })) => Some(DateSpec::Weekday { weekday, nth: number(n)? }),
            ([n], Some(DateSpec::MonthWeekday { month, weekday, .. })) => {
                Some(DateSpec::MonthWeekday { month, weekday, nth: number(n)? })
            }
            _ => None,
        }
    }
}

impl Exception {
    /// Parse an exception line, e.g. `monday 3 - thursday 4 / 2 00:00-24:00`.
    pub fn parse(line: &str) -> Option<Exception> {
        let words: Vec<&str> = line.split_whitespace().collect();
        // the time ranges start with the first word containing a colon
        let times = words.iter().position(|w| w.contains(':'))?;
        let ranges = parse_ranges(&words[times..].concat())?;
        let dates = words[..times].join(" ").replace('/', " / ");
        let mut words: Vec<&str> = dates.split_whitespace().collect();
        let mut skip = 0;
        if let Some(slash) = words.iter().position(|w| *w == "/") {
            let [interval] = words[slash + 1..] else { return None };
            skip = interval.parse().ok()?;
            words.truncate(slash);
        }
        let (first, last) = match words.iter().position(|w| *w == "-") {
            Some(dash) => (&words[..dash], Some(&words[dash + 1..])),
            None => (&words[..], None),
        };
        let start = DateSpec::parse(first, None)?;
        let end = match last {
            Some(last) => DateSpec::parse(last, Some(start))?,
            None if skip <= 1 => start,
            // a skip interval without a range runs to the end of the month, or
            // without end for a calendar date
            None => match start {
                DateSpec::Calendar(_) => DateSpec::Calendar(NaiveDate::MAX),
                DateSpec::MonthDate { month, .. } | DateSpec::MonthWeekday { month, .. } => {
                    DateSpec::MonthDate { month, day: -1 }
                }
                DateSpec::MonthDay { .. } | DateSpec::Weekday { .. } => DateSpec::MonthDay { day: -1 },
            },
        };
        Some(Exception { start, end, skip, ranges })
    }

    /// Whether `date` is one of the days of this exception.
    fn matches(&self, date: NaiveDate) -> bool {
        let (year, month) = (date.year(), date.month());
        let previous_month = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
        let next = |(y, m): (i32, u32)| if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        // A range may begin in the previous month or year and reach into this one.
        let yearly = matches!(self.start, DateSpec::MonthDate { .. } | DateSpec::MonthWeekday { .. });
        let contexts = if yearly { [(year, month), (year - 1, month)] } else { [(year, month), previous_month] };
        contexts.into_iter().any(|context| {
            let Some(start) = self.start.resolve(context.0, context.1) else { return false };
            let Some(mut end) = self.end.resolve(context.0, context.1) else { return false };
            if end < start {
                let later = if yearly { (context.0 + 1, context.1) } else { next(context) };
                match self.end.resolve(later.0, later.1) {
                    Some(e) if e >= start => end = e,
                    _ => return false,
                }
            }
            start <= date && date <= end && (self.skip <= 1 || (date - start).num_days() % i64::from(self.skip) == 0)
        })
    }
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
                // `monday 00:00-24:00` is a weekday, `monday 3 00:00-24:00` an exception
                let weekday = WEEKDAYS.iter().position(|d| *d == key);
                if let (Some(idx), Some(ranges)) = (weekday, parse_ranges(value)) {
                    self.weekdays[idx] = ranges;
                    return;
                }
                match Exception::parse(&format!("{key} {value}")) {
                    Some(exception) => {
                        // keep the most specific kind first, in file order within a kind
                        let at = self.exceptions.partition_point(|e| e.start.rank() <= exception.start.rank());
                        self.exceptions.insert(at, exception);
                    }
                    None => self.unsupported.push(format!("{key} {value}")),
                }
            }
        }
    }

    fn ranges_for(&self, date: NaiveDate) -> &[Range] {
        if let Some(exception) = self.exceptions.iter().find(|e| e.matches(date)) {
            return &exception.ranges;
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

    /// Whether the period `lines` is active on the given day at noon.
    fn noon(p: &HashMap<String, Timeperiod>, y: i32, mo: u32, d: u32) -> bool {
        is_active(p, Some("p"), local(y, mo, d, 12, 0))
    }

    fn period(lines: &[&str]) -> HashMap<String, Timeperiod> {
        let mut t = Timeperiod { name: "p".into(), ..Default::default() };
        for line in lines {
            let (key, value) = line.split_once(' ').unwrap();
            t.add_attr(key, value);
        }
        assert!(t.unsupported.is_empty(), "{:?}", t.unsupported);
        HashMap::from([("p".to_string(), t)])
    }

    #[test]
    fn days_of_a_month() {
        // 2026: february has 28 days
        let p = period(&["day 1 - 3 00:00-24:00", "day -1 00:00-24:00", "february 10 - 12 00:00-24:00"]);
        assert!(noon(&p, 2026, 2, 1) && noon(&p, 2026, 2, 3) && !noon(&p, 2026, 2, 4));
        assert!(noon(&p, 2026, 2, 28) && !noon(&p, 2026, 2, 27) && noon(&p, 2026, 3, 31));
        assert!(noon(&p, 2026, 2, 11) && !noon(&p, 2026, 3, 11) && noon(&p, 2027, 2, 12));
        // ranges may cross the end of a month or year
        let p = period(&["day 28 - 2 00:00-24:00"]);
        assert!(noon(&p, 2026, 11, 30) && noon(&p, 2026, 12, 2) && !noon(&p, 2026, 12, 3) && noon(&p, 2027, 1, 1));
        let p = period(&["december 24 - january 2 00:00-24:00"]);
        assert!(noon(&p, 2026, 12, 24) && noon(&p, 2027, 1, 2) && !noon(&p, 2027, 1, 3) && !noon(&p, 2026, 12, 23));
    }

    #[test]
    fn nth_weekdays() {
        // november 2026 starts on a sunday; its thursdays are the 5th, 12th, 19th and 26th
        let p = period(&["monday 3 00:00-24:00", "thursday -1 november 00:00-24:00", "sunday 5 00:00-24:00"]);
        assert!(noon(&p, 2026, 11, 16) && !noon(&p, 2026, 11, 9) && noon(&p, 2026, 12, 21));
        assert!(noon(&p, 2026, 11, 26) && !noon(&p, 2026, 11, 19) && !noon(&p, 2026, 12, 31));
        // there is a fifth sunday in november (29th); in december the fourth is the last
        assert!(noon(&p, 2026, 11, 29) && noon(&p, 2026, 12, 27));
        let p = period(&["tuesday 1 april - friday 2 may 00:00-24:00"]);
        assert!(noon(&p, 2026, 4, 7) && noon(&p, 2026, 5, 8) && !noon(&p, 2026, 4, 6) && !noon(&p, 2026, 5, 9));
    }

    #[test]
    fn skip_intervals_and_precedence() {
        let p = period(&["2026-11-01 / 3 00:00-24:00"]);
        assert!(noon(&p, 2026, 11, 1) && !noon(&p, 2026, 11, 2) && noon(&p, 2026, 11, 4) && noon(&p, 2027, 11, 2));
        let p = period(&["2026-11-01 - 2026-11-10 /2 00:00-24:00", "day 20 / 5 00:00-24:00"]);
        assert!(noon(&p, 2026, 11, 9) && !noon(&p, 2026, 11, 10) && !noon(&p, 2026, 11, 11));
        assert!(noon(&p, 2026, 11, 25) && noon(&p, 2026, 11, 30) && !noon(&p, 2026, 11, 26) && !noon(&p, 2026, 12, 5));
        // the calendar date wins over the day of the month, which wins over the weekday
        let p = period(&["wednesday 00:00-24:00", "day 18 13:00-14:00", "2026-11-18 08:00-09:00, 10:00-11:00"]);
        assert!(!noon(&p, 2026, 11, 18) && is_active(&p, Some("p"), local(2026, 11, 18, 10, 30)));
        assert!(!noon(&p, 2026, 12, 18) && is_active(&p, Some("p"), local(2026, 12, 18, 13, 30)));
        assert!(noon(&p, 2026, 11, 25));
    }

    #[test]
    fn invalid_exceptions_are_reported() {
        let mut t = Timeperiod::default();
        for (key, value) in
            [("day", "0 00:00-24:00"), ("monday", "3"), ("someday", "1 00:00-24:00"), ("day", "1 / x 00:00-24:00")]
        {
            t.add_attr(key, value);
        }
        assert_eq!(t.unsupported.len(), 4);
        assert!(t.exceptions.is_empty());
    }
}
