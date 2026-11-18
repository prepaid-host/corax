//! Check results and plugin output parsing.

use crate::model::{CheckType, Target};

/// Identifies the object a result belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultTarget {
    /// Result of a check we executed ourselves. `epoch` is the [`crate::World::epoch`]
    /// the ids belong to; after a configuration reload the names are used instead.
    Id { target: Target, epoch: u64, host: String, service: Option<String> },
    /// Result received by name (spool file, command pipe, Livestatus).
    Name { host: String, service: Option<String> },
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub target: ResultTarget,
    pub check_type: CheckType,
    pub return_code: i32,
    /// Complete plugin output, real newlines (not escaped).
    pub output: String,
    pub start_time: f64,
    pub finish_time: f64,
    pub latency: f64,
    /// `false` if the plugin could not be executed (e.g. missing binary).
    pub exited_ok: bool,
    pub timed_out: bool,
    /// Generation of the schedule entry that triggered this active check.
    pub sched_generation: Option<u32>,
}

impl CheckResult {
    pub fn passive(target: ResultTarget, return_code: i32, output: String, time: f64) -> Self {
        CheckResult {
            target,
            check_type: CheckType::Passive,
            return_code,
            output,
            start_time: time,
            finish_time: time,
            latency: 0.0,
            exited_ok: true,
            timed_out: false,
            sched_generation: None,
        }
    }
}

/// Output split into its Nagios parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedOutput {
    pub short: String,
    pub long: String,
    pub perf: String,
}

/// Split plugin output according to the Nagios plugin API:
///
/// ```text
/// SHORT OUTPUT | PERFDATA
/// LONG OUTPUT LINE 1
/// LONG OUTPUT LINE N | PERFDATA 2
/// PERFDATA 3
/// ```
pub fn parse_output(raw: &str) -> ParsedOutput {
    let raw = raw.trim_end_matches(['\n', '\r']);
    let (first, rest) = match raw.split_once('\n') {
        Some((f, r)) => (f, Some(r)),
        None => (raw, None),
    };
    let mut perf_parts: Vec<&str> = Vec::new();
    let short = match first.split_once('|') {
        Some((s, p)) => {
            perf_parts.push(p.trim());
            s.trim_end().to_string()
        }
        None => first.trim_end().to_string(),
    };
    let mut long = String::new();
    if let Some(rest) = rest {
        match rest.split_once('|') {
            Some((l, p)) => {
                long = l.trim_end_matches(['\n', ' ']).to_string();
                perf_parts.extend(p.split('\n').map(str::trim));
            }
            None => long = rest.to_string(),
        }
    }
    let perf = perf_parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
    ParsedOutput { short, long, perf }
}

/// Replace escaped `\n` sequences (passive results) with real newlines and `\\` with `\`.
pub fn unescape_newlines(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                    continue;
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// Escape real newlines as `\n` (for log files and retention).
pub fn escape_newlines(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_only() {
        let p = parse_output("OK - all fine\n");
        assert_eq!(p, ParsedOutput { short: "OK - all fine".into(), ..Default::default() });
    }

    #[test]
    fn short_and_perf() {
        let p = parse_output("OK - 127.0.0.1 rta 0.011ms|rta=0.011ms;200;500;0; pl=0%;80;100;;");
        assert_eq!(p.short, "OK - 127.0.0.1 rta 0.011ms");
        assert_eq!(p.perf, "rta=0.011ms;200;500;0; pl=0%;80;100;;");
    }

    #[test]
    fn checkmk_style_long_output_with_trailing_perf() {
        // Checkmk: "summary\ndetails|metrics"
        let p = parse_output("Total: 12%\nRAM: 1 GiB\nSwap: 0 B|mem_used=123;;;0;456");
        assert_eq!(p.short, "Total: 12%");
        assert_eq!(p.long, "RAM: 1 GiB\nSwap: 0 B");
        assert_eq!(p.perf, "mem_used=123;;;0;456");
    }

    #[test]
    fn nagios_multi_perf() {
        let p = parse_output("DISK OK|/=2643MB;5948;5958;0;5968\n/ 15272 MB (77%);\n/boot 68 MB (69%);| /boot=68MB;88;93;0;98\n/home=69357MB;253404;253409;0;253414");
        assert_eq!(p.short, "DISK OK");
        assert_eq!(p.long, "/ 15272 MB (77%);\n/boot 68 MB (69%);");
        assert_eq!(p.perf, "/=2643MB;5948;5958;0;5968 /boot=68MB;88;93;0;98 /home=69357MB;253404;253409;0;253414");
    }

    #[test]
    fn unescape() {
        assert_eq!(unescape_newlines("a\\nb\\\\n"), "a\nb\\n");
        assert_eq!(escape_newlines("a\nb\\"), "a\\nb\\\\");
    }
}
