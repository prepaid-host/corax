//! Reader for the Nagios check result spool directory (`check_result_path`).
//!
//! Checkmk's default `check_submission = "file"` writes one file `cXXXXXX` per host
//! check run containing all passive service results, followed by an empty marker
//! file `cXXXXXX.ok`. Only files with a marker are processed; both are deleted after
//! reading.

use std::path::{Path, PathBuf};

use crate::checkresult::{unescape_newlines, CheckResult, ResultTarget};
use crate::model::CheckType;

/// Parse the content of one check result file.
pub fn parse_file(text: &str) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for block in text.split("\n\n") {
        let mut host = None;
        let mut service = None;
        let mut rc = 3;
        let mut output = String::new();
        let mut start = 0.0;
        let mut finish = 0.0;
        let mut latency = 0.0;
        let mut check_type = CheckType::Passive;
        let mut exited_ok = true;
        let mut early_timeout = false;
        for line in block.lines() {
            if line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').unwrap_or((line, ""));
            match key {
                "host_name" => host = Some(value.to_string()),
                "service_description" => service = Some(value.to_string()),
                "return_code" => rc = value.trim().parse().unwrap_or(3),
                "output" => output = unescape_newlines(value),
                "start_time" => start = value.trim().parse().unwrap_or(0.0),
                "finish_time" => finish = value.trim().parse().unwrap_or(0.0),
                "latency" => latency = value.trim().parse().unwrap_or(0.0),
                "check_type" => check_type = if value.trim() == "0" { CheckType::Active } else { CheckType::Passive },
                "exited_ok" => exited_ok = value.trim() != "0",
                "early_timeout" => early_timeout = value.trim() == "1",
                _ => {}
            }
        }
        let Some(host) = host else { continue };
        results.push(CheckResult {
            target: ResultTarget::Name { host, service },
            check_type,
            return_code: rc,
            output,
            start_time: start,
            finish_time: if finish > 0.0 { finish } else { start },
            latency,
            exited_ok,
            timed_out: early_timeout,
            sched_generation: None,
        });
    }
    results
}

/// Read and remove all complete result files. Files older than `max_age` seconds are
/// discarded unread.
pub fn reap(dir: &Path, now: i64, max_age: i64) -> Vec<CheckResult> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            name.starts_with('c') && !name.contains('.')
        })
        .collect();
    files.sort();
    let mut results = Vec::new();
    for file in files {
        let ok = file.with_extension("ok");
        // `with_extension` would strip a dot; names have none, so this appends `.ok`.
        if !ok.exists() {
            continue;
        }
        let too_old = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| max_age > 0 && now - d.as_secs() as i64 > max_age);
        if !too_old {
            match std::fs::read_to_string(&file) {
                Ok(text) => results.extend(parse_file(&text)),
                Err(e) => tracing::warn!("cannot read check result file {}: {e}", file.display()),
            }
        } else {
            tracing::warn!("discarding stale check result file {}", file.display());
        }
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&ok);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKMK_FILE: &str = "host_name=db01\nservice_description=Memory\ncheck_type=1\ncheck_options=0\nreschedule_check\nlatency=0.0\nstart_time=1795003818.0\nfinish_time=1795003818.0\nreturn_code=1\noutput=RAM: 42%\\nSwap: 0|mem=1;;;;\n\nhost_name=db01\nservice_description=CPU load\ncheck_type=1\ncheck_options=0\nreschedule_check\nlatency=0.0\nstart_time=1795003818.0\nfinish_time=1795003818.0\nreturn_code=0\noutput=15 min load: 0.1\n\n";

    #[test]
    fn parses_checkmk_submitter_format() {
        let r = parse_file(CHECKMK_FILE);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].target, ResultTarget::Name { host: "db01".into(), service: Some("Memory".into()) });
        assert_eq!(r[0].return_code, 1);
        assert_eq!(r[0].output, "RAM: 42%\nSwap: 0|mem=1;;;;");
        assert_eq!(r[0].check_type, CheckType::Passive);
        assert_eq!(r[1].finish_time, 1795003818.0);
    }

    #[test]
    fn reaps_only_complete_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cabc123"), CHECKMK_FILE).unwrap();
        std::fs::write(dir.path().join("cabc123.ok"), "").unwrap();
        std::fs::write(dir.path().join("cincomp"), CHECKMK_FILE).unwrap();
        let r = reap(dir.path(), crate::now(), 3600);
        assert_eq!(r.len(), 2);
        assert!(!dir.path().join("cabc123").exists());
        assert!(dir.path().join("cincomp").exists());
    }
}
