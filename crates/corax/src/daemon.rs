//! Process lifecycle: configuration verification, daemonization, runtime wiring.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use tokio::sync::mpsc;

use corax_config::{LoadedConfig, Severity};
use corax_core::engine::{Control, Engine, EngineOptions};
use corax_core::{Paths, Shared, World};

use crate::cli::Args;

/// `-v`: verify like `nagios -v` (exit 0 = valid, 1 = invalid).
pub fn verify(config: &Path) -> ExitCode {
    println!("corax {} — verifying {}\n", env!("CARGO_PKG_VERSION"), config.display());
    match corax_config::load(config) {
        Ok(cfg) => {
            let o = &cfg.objects;
            println!("Reading configuration data...");
            println!("   Read main config file okay...");
            println!("   Read object config files okay...\n");
            println!("Running pre-flight check on configuration data...\n");
            println!("Checked {} services.", o.services.len());
            println!("Checked {} hosts.", o.hosts.len());
            println!("Checked {} host groups.", o.hostgroups.len());
            println!("Checked {} service groups.", o.servicegroups.len());
            println!("Checked {} contacts.", o.contacts.len());
            println!("Checked {} contact groups.", o.contactgroups.len());
            println!("Checked {} commands.", o.commands.len());
            println!("Checked {} time periods.", o.timeperiods.len());
            for w in cfg.warnings.iter().filter(|d| d.severity == Severity::Warning) {
                println!("{w}");
            }
            println!("\nTotal Warnings: {}", cfg.warnings.len());
            println!("Total Errors:   0\n");
            println!("Things look okay - No serious problems were detected during the pre-flight check");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("{e}\n");
            println!("***> One or more problems was encountered while processing the config files...");
            ExitCode::from(1)
        }
    }
}

/// Classic double fork. Must run before any threads exist.
fn daemonize() -> std::io::Result<()> {
    // SAFETY: called before the async runtime (or any other thread) is started.
    unsafe {
        match libc::fork() {
            -1 => return Err(std::io::Error::last_os_error()),
            0 => {}
            _ => libc::_exit(0),
        }
        if libc::setsid() == -1 {
            return Err(std::io::Error::last_os_error());
        }
        match libc::fork() {
            -1 => return Err(std::io::Error::last_os_error()),
            0 => {}
            _ => libc::_exit(0),
        }
        // Become a process group leader: OMD's init script stops the core with
        // `kill -TERM -<pid>` (process group).
        libc::setpgid(0, 0);
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
        libc::umask(0o007);
    }
    Ok(())
}

fn init_tracing(args: &Args, diag_file: Option<&Path>) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    match diag_file.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(file) => builder.with_ansi(false).with_writer(std::sync::Mutex::new(file)).init(),
        None => builder.with_writer(std::io::stderr).init(),
    }
}

fn load(config: &Path) -> anyhow::Result<LoadedConfig> {
    corax_config::load(config).with_context(|| format!("loading {}", config.display()))
}

/// Run the core (optionally as daemon) until SIGTERM/SIGINT.
pub fn run(config: &Path, args: &Args) -> ExitCode {
    // Load before daemonizing so configuration errors are visible to the caller.
    let loaded = match load(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e:#}");
            return ExitCode::from(1);
        }
    };
    let paths = Paths::from_main(&loaded.main);
    // Lock before forking so a second instance fails visibly; flock survives fork.
    let mut lock = match LockFile::acquire(paths.lock_file.clone()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::from(1);
        }
    };
    if args.daemon {
        if let Err(e) = daemonize() {
            eprintln!("Error: cannot daemonize: {e}");
            return ExitCode::from(1);
        }
    }
    let diag = args.diag_log.clone().or_else(|| {
        args.daemon.then(|| paths.log_file.as_ref().and_then(|l| l.parent()).map(|d| d.join("corax.log"))).flatten()
    });
    init_tracing(args, diag.as_deref());
    lock.write_pid();
    for w in &loaded.warnings {
        tracing::warn!("{w}");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("corax").build() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("cannot start runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let result = runtime.block_on(serve(config.to_path_buf(), loaded, paths, args.clone()));
    drop(lock);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::from(1)
        }
    }
}

/// Exclusive lock on `lock_file` (like Nagios): prevents two cores on one site.
struct LockFile {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
}

impl LockFile {
    /// Acquire the lock or report the PID of the instance holding it.
    fn acquire(path: Option<PathBuf>) -> Result<Self, String> {
        use std::os::fd::AsRawFd;
        let Some(p) = path.clone() else { return Ok(LockFile { path, file: None }) };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)
            .map_err(|e| format!("cannot open lock file {}: {e}", p.display()))?;
        // SAFETY: valid fd owned by `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let holder = std::fs::read_to_string(&p).unwrap_or_default();
            return Err(format!(
                "another monitoring core is already running for this site (lock file {}, pid {})",
                p.display(),
                holder.trim()
            ));
        }
        Ok(LockFile { path, file: Some(file) })
    }

    /// Write our PID (after daemonizing).
    fn write_pid(&mut self) {
        use std::io::{Seek, Write};
        if let Some(f) = self.file.as_mut() {
            let _ = f.set_len(0);
            let _ = f.rewind();
            let _ = writeln!(f, "{}", std::process::id());
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if let (Some(f), Some(_)) = (self.file.as_ref(), self.path.as_ref()) {
            let _ = f.set_len(0);
        }
    }
}

async fn serve(config: PathBuf, loaded: LoadedConfig, paths: Paths, args: Args) -> anyhow::Result<()> {
    let now = corax_core::now();
    let world = World::new(&loaded, now);
    tracing::info!(
        "corax {} starting: {} hosts, {} services (pid {})",
        env!("CARGO_PKG_VERSION"),
        world.hosts.len(),
        world.services.len(),
        std::process::id()
    );
    let (shared, rx) = Shared::new(world, paths.clone());

    let options = EngineOptions { check_helper: args.check_helper, max_concurrent_checks: args.max_concurrent_checks };
    let loader_cfg = config.clone();
    let mut engine = Engine::new(shared.clone(), rx, options, Box::new(move || load(&loader_cfg)));
    engine.startup();

    let mut tasks = Vec::new();
    if let Some(socket) = paths.livestatus_socket.clone() {
        let s = shared.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = corax_livestatus::serve(s, socket.clone()).await {
                tracing::error!("livestatus on {} failed: {e:#}", socket.display());
            }
        }));
    } else {
        tracing::warn!("no Livestatus broker_module configured; Livestatus is disabled");
    }
    if let Some(cmd) = paths.command_file.clone() {
        let s = shared.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = corax_core::cmdpipe::serve(s, &cmd).await {
                tracing::error!("command pipe {} failed: {e}", cmd.display());
            }
        }));
    }

    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    tasks.push(tokio::spawn(signals(ctl_tx)));

    engine.run(ctl_rx).await;

    for t in tasks {
        t.abort();
    }
    if let Some(cmd) = &paths.command_file {
        let _ = std::fs::remove_file(cmd);
    }
    tracing::info!("corax stopped");
    Ok(())
}

async fn signals(tx: mpsc::UnboundedSender<Control>) {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut hup), Ok(mut term), Ok(mut int)) =
        (signal(SignalKind::hangup()), signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
    else {
        tracing::error!("cannot install signal handlers");
        return;
    };
    loop {
        let ctl = tokio::select! {
            _ = hup.recv() => Control::Reload,
            _ = term.recv() => Control::Shutdown,
            _ = int.recv() => Control::Shutdown,
        };
        if tx.send(ctl).is_err() || ctl == Control::Shutdown {
            return;
        }
    }
}
