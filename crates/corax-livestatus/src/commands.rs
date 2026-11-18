//! `COMMAND` requests.
//!
//! Most commands are Nagios external commands and are forwarded to the engine. A few
//! are Livestatus extensions handled here, like in the original NEB module:
//! `MK_LOGWATCH_ACKNOWLEDGE`, `DEL_CRASH_REPORT` and `EC_*` (Event Console).

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use corax_core::{CommandSource, Shared};

/// Split `[1700000000] NAME;arg;arg` into `(NAME, args)`.
fn split_command(cmd: &str) -> (&str, &str) {
    let body = match cmd.strip_prefix('[') {
        Some(r) => r.split_once(']').map(|(_, rest)| rest.trim_start()).unwrap_or(r),
        None => cmd,
    };
    body.split_once(';').unwrap_or((body, ""))
}

/// A path component that cannot escape its directory.
fn safe_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\0')
}

pub fn handle(shared: &Shared, cmd: &str) {
    let (name, args) = split_command(cmd);
    let paths = shared.paths();
    match name {
        "MK_LOGWATCH_ACKNOWLEDGE" => {
            // MK_LOGWATCH_ACKNOWLEDGE;host;\var\log\syslog
            let mut it = args.splitn(2, ';');
            let (Some(host), Some(file)) = (it.next(), it.next()) else {
                tracing::warn!("MK_LOGWATCH_ACKNOWLEDGE expects 2 arguments");
                return;
            };
            if let Some(base) = &paths.mk_logwatch_path {
                if safe_component(host) && safe_component(file) {
                    let _ = std::fs::remove_file(base.join(host).join(file));
                } else {
                    tracing::warn!("MK_LOGWATCH_ACKNOWLEDGE: refusing suspicious path {host:?}/{file:?}");
                }
            }
        }
        "DEL_CRASH_REPORT" => {
            let id = args.trim();
            let valid = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if let (true, Some(base)) = (valid, &paths.crash_reports_path) {
                delete_crash_report(base, id);
            }
        }
        _ if name.starts_with("EC_") => {
            let forwarded = if args.is_empty() {
                format!("COMMAND {}", &name[3..])
            } else {
                format!("COMMAND {};{args}", &name[3..])
            };
            if let Some(sock) = &paths.mkeventd_socket {
                if let Err(e) = send_to_event_console(sock, &forwarded) {
                    tracing::warn!("cannot forward command to the Event Console: {e}");
                }
            }
        }
        _ => shared.submit_command(cmd, CommandSource::Livestatus),
    }
}

fn delete_crash_report(base: &Path, id: &str) {
    let Ok(components) = std::fs::read_dir(base) else { return };
    for comp in components.filter_map(Result::ok) {
        let dir = comp.path().join(id);
        if dir.is_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn send_to_event_console(socket: &Path, command: &str) -> std::io::Result<()> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(command.as_bytes())?;
    s.write_all(b"\n")?;
    s.shutdown(std::net::Shutdown::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands() {
        assert_eq!(split_command("[123] DEL_CRASH_REPORT;abc"), ("DEL_CRASH_REPORT", "abc"));
        assert_eq!(split_command("ENABLE_NOTIFICATIONS"), ("ENABLE_NOTIFICATIONS", ""));
    }

    #[test]
    fn path_safety() {
        assert!(safe_component("\\var\\log\\syslog"));
        assert!(!safe_component("../etc"));
        assert!(!safe_component(".."));
    }
}
