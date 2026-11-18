//! Client and supervisor for the Python check helper ("zygote").
//!
//! See `check_helper.py` for the rationale. The helper is started lazily the first
//! time a Checkmk precompiled host check (`python3 -P …/host_checks/<host>`) is due,
//! using the very same interpreter. Until it reports `READY` (warm-up takes a few
//! seconds) and whenever it is unavailable, checks transparently fall back to a
//! regular fork/exec.
//!
//! A failed request (connect error, fork failure in the helper, …) suspends the helper
//! only briefly; after [`MAX_FAILURES`] consecutive failures it is restarted. The helper
//! must never stay disabled while its process is alive: every fallback forks a full
//! Python interpreter, which is exactly the load the helper exists to avoid.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::exec::{kill_group, ExecOutput};

const SCRIPT: &str = include_str!("check_helper.py");

/// Consecutive failed requests after which the helper is restarted.
const MAX_FAILURES: u32 = 3;
/// How long checks bypass the helper after a failed request.
const SUSPEND: Duration = Duration::from_secs(2);

/// Failure bookkeeping between the helper being started and being usable.
#[derive(Debug, Default)]
struct Health {
    failures: u32,
    suspended_until: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum OnFailure {
    Suspend,
    Restart,
}

impl Health {
    fn usable(&self, now: Instant) -> bool {
        self.suspended_until.is_none_or(|t| now >= t)
    }

    fn success(&mut self) {
        self.failures = 0;
        self.suspended_until = None;
    }

    fn failure(&mut self, now: Instant) -> OnFailure {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            *self = Health::default();
            return OnFailure::Restart;
        }
        self.suspended_until = Some(now + SUSPEND);
        OnFailure::Suspend
    }
}

/// Is `argv` a Checkmk precompiled host check invocation?
pub fn is_host_check(argv: &[String]) -> bool {
    argv.len() >= 3
        && Path::new(&argv[0]).file_name().is_some_and(|n| n.to_string_lossy().starts_with("python3"))
        && argv[1] == "-P"
        && Path::new(&argv[2]).parent().is_some_and(|p| p.ends_with("host_checks"))
        && argv[3..].iter().all(|a| matches!(a.as_str(), "-v" | "-vv" | "-d"))
}

struct Running {
    child: Child,
    python: String,
    host_checks_dir: PathBuf,
}

/// Supervised check helper process.
pub struct CheckHelper {
    enabled: bool,
    socket: PathBuf,
    script: PathBuf,
    ready: Arc<AtomicBool>,
    /// Incremented for every started helper; a stale stdout reader must not touch `ready`.
    generation: Arc<AtomicU64>,
    health: Mutex<Health>,
    running: Mutex<Option<Running>>,
    last_start: Mutex<Option<Instant>>,
    pub served: AtomicU64,
    pub fallbacks: AtomicU64,
}

impl CheckHelper {
    /// `work_dir` receives the helper script and socket (e.g. `$OMD_ROOT/tmp/nagios`).
    pub fn new(enabled: bool, work_dir: &Path) -> Self {
        CheckHelper {
            enabled,
            socket: work_dir.join("corax-check-helper.sock"),
            script: work_dir.join("corax-check-helper.py"),
            ready: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            health: Mutex::new(Health::default()),
            running: Mutex::new(None),
            last_start: Mutex::new(None),
            served: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn is_usable(&self) -> bool {
        self.is_ready() && self.health.lock().usable(Instant::now())
    }

    /// A request failed before the helper took responsibility for the check.
    fn request_failed(&self, error: &str) {
        match self.health.lock().failure(Instant::now()) {
            OnFailure::Suspend => {
                tracing::warn!("check helper request failed ({error}); using fork/exec for {}s", SUSPEND.as_secs());
            }
            OnFailure::Restart => {
                tracing::warn!("check helper failed {MAX_FAILURES} times in a row ({error}); restarting it");
                self.ready.store(false, Ordering::Release);
                if let Some(mut r) = self.running.lock().take() {
                    let _ = r.child.start_kill();
                }
            }
        }
    }

    /// Make sure a helper for `python`/`dir` is running (starting it if needed).
    fn ensure_started(&self, python: &str, host_checks_dir: &Path) {
        let mut running = self.running.lock();
        if let Some(r) = running.as_mut() {
            let alive = matches!(r.child.try_wait(), Ok(None));
            if alive && r.python == python && r.host_checks_dir == host_checks_dir {
                return;
            }
            if alive {
                let _ = r.child.start_kill();
            } else {
                tracing::warn!("check helper exited; restarting");
            }
            self.ready.store(false, Ordering::Release);
            *running = None;
        }
        // Back off after failures.
        let mut last = self.last_start.lock();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return;
        }
        *last = Some(Instant::now());
        if let Err(e) = std::fs::write(&self.script, SCRIPT) {
            tracing::warn!("cannot write check helper script {}: {e}", self.script.display());
            return;
        }
        let mut cmd = Command::new(python);
        cmd.arg(&self.script)
            .arg(&self.socket)
            .arg(host_checks_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("cannot start check helper with {python}: {e}");
                return;
            }
        };
        tracing::info!("starting check helper (pid {:?}) for {}", child.id(), host_checks_dir.display());
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.health.lock() = Health::default();
        if let Some(stdout) = child.stdout.take() {
            let ready = self.ready.clone();
            let current = self.generation.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim() == "READY" && current.load(Ordering::Acquire) == generation {
                        tracing::info!("check helper is ready");
                        ready.store(true, Ordering::Release);
                    }
                }
                if current.load(Ordering::Acquire) == generation {
                    ready.store(false, Ordering::Release);
                }
            });
        }
        *running = Some(Running { child, python: python.to_string(), host_checks_dir: host_checks_dir.to_path_buf() });
    }

    /// Stop the helper (on reload/shutdown). It is restarted lazily.
    pub fn stop(&self) {
        self.ready.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(mut r) = self.running.lock().take() {
            let _ = r.child.start_kill();
        }
        *self.last_start.lock() = None;
    }

    /// Run a host check through the helper. `None` means "not handled, use exec".
    pub async fn run(&self, argv: &[String], timeout: Duration) -> Option<ExecOutput> {
        if !self.enabled || !is_host_check(argv) {
            return None;
        }
        let dir = Path::new(&argv[2]).parent()?.to_path_buf();
        self.ensure_started(&argv[0], &dir);
        if !self.is_usable() {
            self.fallbacks.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let started = Instant::now();
        match tokio::time::timeout(timeout, self.roundtrip(argv)).await {
            Ok(Ok((_, rc, output))) => {
                self.served.fetch_add(1, Ordering::Relaxed);
                self.health.lock().success();
                Some(ExecOutput {
                    return_code: rc,
                    output,
                    exited_ok: true,
                    timed_out: false,
                    duration: started.elapsed(),
                })
            }
            Ok(Err((pid, e))) => {
                if let Some(pid) = pid {
                    kill_group(pid);
                    // The check was started; report the failure rather than running it twice.
                    return Some(ExecOutput {
                        return_code: 3,
                        output: format!("UNKNOWN - check helper failed: {e}"),
                        exited_ok: false,
                        timed_out: false,
                        duration: started.elapsed(),
                    });
                }
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                self.request_failed(&e);
                None
            }
            Err(_) => Some(ExecOutput {
                return_code: 3,
                output: String::new(),
                exited_ok: false,
                timed_out: true,
                duration: started.elapsed(),
            }),
        }
    }

    async fn roundtrip(&self, argv: &[String]) -> Result<(u32, i32, String), (Option<u32>, String)> {
        let stream = UnixStream::connect(&self.socket).await.map_err(|e| (None, e.to_string()))?;
        let (rd, mut wr) = stream.into_split();
        let req = serde_json::json!({ "argv": argv }).to_string() + "\n";
        wr.write_all(req.as_bytes()).await.map_err(|e| (None, e.to_string()))?;
        let mut lines = BufReader::new(rd).lines();
        let first = lines.next_line().await.map_err(|e| (None, e.to_string()))?.unwrap_or_default();
        let pid: u32 = first
            .strip_prefix("PID ")
            .and_then(|p| p.trim().parse().ok())
            .ok_or_else(|| (None, format!("unexpected helper response '{first}'")))?;
        // If we are cancelled (timeout) the child must not survive.
        let guard = KillOnDrop(Some(pid));
        let line = lines.next_line().await.map_err(|e| (Some(pid), e.to_string()))?;
        let line = line.ok_or_else(|| (Some(pid), "helper child closed the connection".to_string()))?;
        let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| (Some(pid), e.to_string()))?;
        std::mem::forget(guard);
        let rc = v.get("rc").and_then(|r| r.as_i64()).unwrap_or(3) as i32;
        let output = v.get("output").and_then(|o| o.as_str()).unwrap_or_default().to_string();
        Ok((pid, rc, output))
    }
}

struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_checkmk_host_checks() {
        assert!(is_host_check(&v(&[
            "/omd/sites/x/bin/python3",
            "-P",
            "/omd/sites/x/var/check_mk/core/helper_config/latest/host_checks/h1"
        ])));
        assert!(is_host_check(&v(&["python3", "-P", "/a/host_checks/h1", "-v"])));
        assert!(!is_host_check(&v(&["python3", "-P", "/a/other/h1"])));
        assert!(!is_host_check(&v(&["python3", "/a/host_checks/h1"])));
        assert!(!is_host_check(&v(&["python3", "-P", "/a/host_checks/h1", "--evil"])));
    }

    /// Regression: one failed request used to disable a live helper forever, turning
    /// every host check into a full Python fork/exec.
    #[test]
    fn failures_suspend_briefly_then_restart() {
        let mut h = Health::default();
        let t = Instant::now();
        assert!(h.usable(t));
        assert_eq!(h.failure(t), OnFailure::Suspend);
        assert!(!h.usable(t));
        assert!(h.usable(t + SUSPEND));
        h.success();
        assert!(h.usable(t));
        for _ in 1..MAX_FAILURES {
            assert_eq!(h.failure(t), OnFailure::Suspend);
        }
        assert_eq!(h.failure(t), OnFailure::Restart);
        assert!(h.usable(t), "a restarted helper starts healthy");
    }
}
