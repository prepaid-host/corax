//! Performance data output compatible with PNP4Nagios' `npcdmod` broker module.
//!
//! Lines are appended to `perfdata_file`; every `perfdata_file_processing_interval`
//! seconds the file is moved to `perfdata_spool_dir/perfdata.<timestamp>`, where
//! `npcd` picks it up and feeds the RRDs (Checkmk Community graphs).

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

pub struct PerfdataWriter {
    file: PathBuf,
    spool: PathBuf,
    interval: i64,
    next_rotation: i64,
    writer: Option<BufWriter<File>>,
    pending: bool,
}

/// Remove characters that would break the TAB/`::` separated format.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

impl PerfdataWriter {
    pub fn new(file: PathBuf, spool: PathBuf, interval: i64, now: i64) -> Self {
        PerfdataWriter {
            file,
            spool,
            interval: interval.max(1),
            next_rotation: now + interval.max(1),
            writer: None,
            pending: false,
        }
    }

    fn writer(&mut self) -> Option<&mut BufWriter<File>> {
        if self.writer.is_none() {
            match OpenOptions::new().create(true).append(true).open(&self.file) {
                Ok(f) => self.writer = Some(BufWriter::with_capacity(64 * 1024, f)),
                Err(e) => {
                    tracing::error!("cannot open perfdata file {}: {e}", self.file.display());
                    return None;
                }
            }
        }
        self.writer.as_mut()
    }

    pub fn host(&mut self, time: i64, host: &str, perf: &str, command: &str, state: u8, state_type: u8) {
        let line = format!(
            "DATATYPE::HOSTPERFDATA\tTIMET::{time}\tHOSTNAME::{}\tHOSTPERFDATA::{}\tHOSTCHECKCOMMAND::{}\tHOSTSTATE::{state}\tHOSTSTATETYPE::{state_type}\n",
            clean(host),
            clean(perf),
            clean(command)
        );
        self.append(&line);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn service(&mut self, time: i64, host: &str, desc: &str, perf: &str, command: &str, state: u8, state_type: u8) {
        let line = format!(
            "DATATYPE::SERVICEPERFDATA\tTIMET::{time}\tHOSTNAME::{}\tSERVICEDESC::{}\tSERVICEPERFDATA::{}\tSERVICECHECKCOMMAND::{}\tSERVICESTATE::{state}\tSERVICESTATETYPE::{state_type}\n",
            clean(host),
            clean(desc),
            clean(perf),
            clean(command)
        );
        self.append(&line);
    }

    fn append(&mut self, line: &str) {
        if let Some(w) = self.writer() {
            if w.write_all(line.as_bytes()).is_ok() {
                self.pending = true;
            }
        }
    }

    pub fn flush(&mut self) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.flush();
        }
    }

    /// Move the current file into the spool directory if the interval elapsed.
    pub fn maybe_rotate(&mut self, now: i64) {
        if now < self.next_rotation {
            return;
        }
        self.next_rotation = now + self.interval;
        if !self.pending {
            return;
        }
        self.flush();
        self.writer = None;
        self.pending = false;
        let target = self.spool.join(format!("perfdata.{now}"));
        if let Err(e) = std::fs::rename(&self.file, &target) {
            tracing::warn!("cannot move perfdata file to {}: {e}", target.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npcdmod_format_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("perfdata.dump");
        let spool = dir.path().join("spool");
        std::fs::create_dir(&spool).unwrap();
        let mut w = PerfdataWriter::new(file.clone(), spool.clone(), 15, 100);
        w.service(101, "db01", "CPU utilization", "util=24.7;;;0;", "check_mk-cpu_utilization_os", 0, 1);
        w.flush();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "DATATYPE::SERVICEPERFDATA\tTIMET::101\tHOSTNAME::db01\tSERVICEDESC::CPU utilization\tSERVICEPERFDATA::util=24.7;;;0;\tSERVICECHECKCOMMAND::check_mk-cpu_utilization_os\tSERVICESTATE::0\tSERVICESTATETYPE::1\n"
        );
        w.maybe_rotate(110);
        assert!(file.exists());
        w.maybe_rotate(115);
        assert!(!file.exists());
        assert!(spool.join("perfdata.115").exists());
    }
}
