//! Process execution for checks, notifications and event handlers.
//!
//! Like Nagios, a command line is passed to `/bin/sh -c` only if it contains shell
//! meta characters. Unlike Nagios, plain single/double quotes are handled by corax
//! itself, so the very common Checkmk command
//! `python3 -P …/host_checks/"myhost"` is executed without an intermediate shell.
//! Every child runs in its own process group; on timeout the whole group is killed.

use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum number of output bytes kept from a plugin.
pub const MAX_OUTPUT: usize = 1024 * 1024;

const SHELL_META: &[char] =
    &['!', '$', '^', '&', '*', '(', ')', '~', '[', ']', '\\', '|', '{', '}', ';', '<', '>', '?', '`'];

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub return_code: i32,
    pub output: String,
    /// The process was started and exited normally (not signalled / not failed to spawn).
    pub exited_ok: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

/// How a command line will be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Direct(Vec<String>),
    Shell(String),
}

/// Split a command line into argv if it can be done without a shell.
pub fn plan(cmdline: &str) -> Invocation {
    let trimmed = cmdline.trim();
    // Leading `VAR=value` assignments need a shell.
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if trimmed.contains(SHELL_META) || first.contains('=') {
        return Invocation::Shell(trimmed.to_string());
    }
    match split_words(trimmed) {
        Some(argv) if !argv.is_empty() => Invocation::Direct(argv),
        _ => Invocation::Shell(trimmed.to_string()),
    }
}

/// POSIX-like word splitting supporting '…' and "…" quoting (no escapes, no expansion).
fn split_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some(q) if q == c => break,
                        Some(other) => cur.push(other),
                        None => return None, // unbalanced quote → let the shell complain
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

fn build_command(inv: &Invocation) -> Command {
    match inv {
        Invocation::Direct(argv) => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        Invocation::Shell(line) => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(line);
            c
        }
    }
}

/// Kill a whole process group.
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: plain syscall; a negative pid addresses the process group.
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGKILL);
        }
    }
}

/// Run a command line with a timeout and capture its standard output.
pub async fn run(cmdline: &str, timeout: Duration, env: &[(String, String)]) -> ExecOutput {
    run_invocation(&plan(cmdline), timeout, env).await
}

pub async fn run_invocation(inv: &Invocation, timeout: Duration, env: &[(String, String)]) -> ExecOutput {
    let started = Instant::now();
    let mut cmd = build_command(inv);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let rc = if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
            return ExecOutput {
                return_code: rc,
                output: format!("(Return code of {rc} is out of bounds - plugin may be missing: {e})"),
                exited_ok: false,
                timed_out: false,
                duration: started.elapsed(),
            };
        }
    };
    let pid = child.id().unwrap_or(0);
    let mut stdout = child.stdout.take().expect("stdout is piped");

    let work = async {
        let mut buf = Vec::with_capacity(512);
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        let status = child.wait().await;
        (buf, status)
    };

    match tokio::time::timeout(timeout, work).await {
        Ok((buf, status)) => {
            // Background processes left behind in the group are cleaned up as well.
            kill_group(pid);
            let output = String::from_utf8_lossy(&buf).into_owned();
            match status {
                Ok(st) => match st.code() {
                    Some(rc) => ExecOutput {
                        return_code: rc,
                        output,
                        exited_ok: true,
                        timed_out: false,
                        duration: started.elapsed(),
                    },
                    None => ExecOutput {
                        return_code: 3,
                        output: if output.is_empty() { "(Plugin was killed by a signal)".into() } else { output },
                        exited_ok: false,
                        timed_out: false,
                        duration: started.elapsed(),
                    },
                },
                Err(e) => ExecOutput {
                    return_code: 3,
                    output: format!("(Error waiting for plugin: {e})"),
                    exited_ok: false,
                    timed_out: false,
                    duration: started.elapsed(),
                },
            }
        }
        Err(_) => {
            kill_group(pid);
            ExecOutput {
                return_code: 3,
                output: String::new(),
                exited_ok: false,
                timed_out: true,
                duration: started.elapsed(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning() {
        assert_eq!(plan("/bin/echo a b"), Invocation::Direct(vec!["/bin/echo".into(), "a".into(), "b".into()]));
        assert_eq!(
            plan("/omd/sites/x/bin/python3 -P /omd/sites/x/var/host_checks/\"my host\""),
            Invocation::Direct(vec![
                "/omd/sites/x/bin/python3".into(),
                "-P".into(),
                "/omd/sites/x/var/host_checks/my host".into()
            ])
        );
        assert!(matches!(plan("echo a && exit 1"), Invocation::Shell(_)));
        assert!(matches!(plan("NOTIFY_X='1' check_mk --notify"), Invocation::Shell(_)));
        assert!(matches!(plan("echo 'unbalanced"), Invocation::Shell(_)));
    }

    #[tokio::test]
    async fn runs_and_captures() {
        let out = run("/bin/sh -c 'echo hello; exit 2'", Duration::from_secs(5), &[]).await;
        // contains quotes but no meta → direct exec of /bin/sh with the quoted script
        assert_eq!(out.return_code, 2);
        assert_eq!(out.output, "hello\n");
    }

    #[tokio::test]
    async fn timeout_kills_group() {
        let out = run("sleep 5; echo never", Duration::from_millis(200), &[]).await;
        assert!(out.timed_out);
        assert!(out.duration < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn missing_binary() {
        let out = run("/nonexistent/plugin", Duration::from_secs(1), &[]).await;
        assert_eq!(out.return_code, 127);
        assert!(!out.exited_ok);
        assert!(out.output.contains("plugin may be missing"));
    }
}
