//! `nagios.log` writer with Nagios-compatible line format and archive rotation.
//!
//! Livestatus' `log` and `statehist` tables (and therefore the Checkmk GUI event,
//! notification and availability views) are built from these files, so the format
//! must match Nagios exactly: `[<unix time>] <TYPE>: <fields separated by ;>`.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use chrono::{Datelike, Local, TimeZone, Timelike};

pub struct NagiosLog {
    path: Option<PathBuf>,
    archive: Option<PathBuf>,
    writer: Option<BufWriter<File>>,
    /// Unix time of the next rotation (0 = never).
    next_rotation: i64,
    method: char,
    pub lines_written: u64,
}

/// Start of the next rotation period after `t`.
fn next_rotation_after(t: i64, method: char) -> i64 {
    let Some(now) = Local.timestamp_opt(t, 0).single() else { return 0 };
    let base = now.with_minute(0).and_then(|d| d.with_second(0)).unwrap_or(now);
    let next = match method {
        'h' => base + chrono::Duration::hours(1),
        'd' => base.with_hour(0).unwrap_or(base) + chrono::Duration::days(1),
        'w' => {
            let midnight = base.with_hour(0).unwrap_or(base);
            let days_to_sunday = 7 - midnight.weekday().num_days_from_sunday() as i64;
            midnight + chrono::Duration::days(days_to_sunday)
        }
        'm' => {
            let first = base.with_hour(0).and_then(|d| d.with_day(1)).unwrap_or(base);
            let (y, m) = if first.month() == 12 { (first.year() + 1, 1) } else { (first.year(), first.month() + 1) };
            first
                .with_day(1)
                .and_then(|d| d.with_month(m))
                .and_then(|d| d.with_year(y))
                .unwrap_or(first + chrono::Duration::days(31))
        }
        _ => return 0,
    };
    next.timestamp()
}

impl NagiosLog {
    pub fn new(path: Option<PathBuf>, archive: Option<PathBuf>, method: char, now: i64) -> Self {
        let mut log = NagiosLog {
            path,
            archive,
            writer: None,
            next_rotation: next_rotation_after(now, method),
            method,
            lines_written: 0,
        };
        log.open();
        log
    }

    fn open(&mut self) {
        self.writer = self.path.as_ref().and_then(|p| match OpenOptions::new().create(true).append(true).open(p) {
            Ok(f) => Some(BufWriter::with_capacity(64 * 1024, f)),
            Err(e) => {
                tracing::error!("cannot open log file {}: {e}", p.display());
                None
            }
        });
    }

    /// Append one entry. `text` must not contain newlines (they are escaped).
    pub fn write(&mut self, time: i64, text: &str) {
        if let Some(w) = self.writer.as_mut() {
            let r = if text.contains('\n') {
                writeln!(w, "[{time}] {}", text.replace('\n', "\\n"))
            } else {
                writeln!(w, "[{time}] {text}")
            };
            if let Err(e) = r {
                tracing::error!("cannot write to log file: {e}");
            }
            self.lines_written += 1;
        }
    }

    pub fn flush(&mut self) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.flush();
        }
    }

    pub fn rotation_due(&self, now: i64) -> bool {
        self.next_rotation > 0 && now >= self.next_rotation
    }

    /// Move the current log into the archive (`nagios-MM-DD-YYYY-HH.log`).
    /// The caller writes the `LOG ROTATION`/`CURRENT … STATE` lines afterwards.
    pub fn rotate(&mut self, now: i64) {
        self.flush();
        self.writer = None;
        if let (Some(path), Some(archive)) = (&self.path, &self.archive) {
            // Name after the end of the period, like Nagios.
            let stamp = Local
                .timestamp_opt(self.next_rotation.max(now) - 1, 0)
                .single()
                .map(|d| (d + chrono::Duration::seconds(1)).format("%m-%d-%Y-%H").to_string())
                .unwrap_or_default();
            let target = archive.join(format!("nagios-{stamp}.log"));
            let _ = std::fs::create_dir_all(archive);
            if let Err(e) = std::fs::rename(path, &target) {
                tracing::error!("log rotation to {} failed: {e}", target.display());
            }
        }
        self.next_rotation = next_rotation_after(now, self.method);
        self.open();
    }

    pub fn rotation_method_name(&self) -> &'static str {
        match self.method {
            'h' => "HOURLY",
            'd' => "DAILY",
            'w' => "WEEKLY",
            'm' => "MONTHLY",
            _ => "NONE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("nagios.log");
        let archive = dir.path().join("archive");
        let mut log = NagiosLog::new(Some(log_path.clone()), Some(archive.clone()), 'd', 1_700_000_000);
        log.write(1_700_000_000, "SERVICE ALERT: h;s;OK;HARD;1;fine\nmore");
        log.flush();
        let text = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(text, "[1700000000] SERVICE ALERT: h;s;OK;HARD;1;fine\\nmore\n");
        assert!(log.next_rotation > 1_700_000_000 && log.next_rotation <= 1_700_000_000 + 86400);
        log.rotate(log.next_rotation);
        assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 1);
        log.write(1, "LOG ROTATION: DAILY");
        log.flush();
        assert!(std::fs::read_to_string(&log_path).unwrap().contains("LOG ROTATION"));
    }
}
