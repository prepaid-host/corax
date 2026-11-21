//! The engine task: the only writer of the [`World`].
//!
//! It multiplexes four event sources — inputs (check results, external commands),
//! due checks from the scheduler, a 1 s housekeeping tick and control messages
//! (reload/shutdown) — and turns the [`Effect`]s produced under the lock into I/O.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Notify, Semaphore};

use corax_config::{timeperiod, LoadedConfig};

use crate::checkresult::{CheckResult, ResultTarget};
use crate::checks;
use crate::commands;
use crate::downtime;
use crate::effects::{Ctx, Effect};
use crate::exec::{self, Invocation};
use crate::helper::{self, CheckHelper};
use crate::macros::{host_state_name, service_state_name, state_type_name};
use crate::model::*;
use crate::nagioslog::NagiosLog;
use crate::perfdata::PerfdataWriter;
use crate::retention;
use crate::scheduler::{self, Scheduler};
use crate::{now, now_f64, spool, Input, Paths, Shared, World};

/// Runtime options that are not part of the Nagios configuration.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Use the pre-forking Python helper for Checkmk host checks.
    pub check_helper: bool,
    /// Overrides `max_concurrent_checks` from nagios.cfg (0 = use nagios.cfg / unlimited).
    pub max_concurrent_checks: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions { check_helper: true, max_concurrent_checks: 0 }
    }
}

/// Messages from the process supervisor (signal handlers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Reload,
    Shutdown,
}

/// Loads a fresh configuration on reload.
pub type ConfigLoader = Box<dyn Fn() -> anyhow::Result<LoadedConfig> + Send + Sync>;

const MAX_BATCH: usize = 10_000;
/// Hard cap when neither nagios.cfg nor the options limit concurrency.
const DEFAULT_CONCURRENCY: usize = 2048;

pub struct Engine {
    shared: Arc<Shared>,
    rx: mpsc::UnboundedReceiver<Input>,
    sched: Scheduler,
    log: NagiosLog,
    perf: Option<PerfdataWriter>,
    helper: Arc<CheckHelper>,
    limiter: Arc<Semaphore>,
    reap_now: Arc<Notify>,
    reaper: Option<tokio::task::JoinHandle<()>>,
    options: EngineOptions,
    loader: ConfigLoader,
    next_retention_save: i64,
    next_freshness: i64,
    next_host_freshness: i64,
    /// Last logged state of every timeperiod (`TIMEPERIOD TRANSITION` lines).
    timeperiods: HashMap<String, bool>,
    next_timeperiod_check: i64,
}

fn concurrency(world: &World, options: &EngineOptions) -> usize {
    match (options.max_concurrent_checks, world.settings.max_concurrent_checks) {
        (n, _) if n > 0 => n,
        (_, n) if n > 0 => n,
        _ => DEFAULT_CONCURRENCY,
    }
}

impl Engine {
    pub fn new(
        shared: Arc<Shared>,
        rx: mpsc::UnboundedReceiver<Input>,
        options: EngineOptions,
        loader: ConfigLoader,
    ) -> Self {
        let paths = shared.paths();
        let now = now();
        let (rotation, permits) = {
            let w = shared.world.read();
            (w.settings.log_rotation_method, concurrency(&w, &options))
        };
        // The helper's script and socket live in the site's private temp_path; without one
        // (no predictable names in a shared /tmp) the helper is disabled.
        let options = EngineOptions { check_helper: options.check_helper && paths.temp_path.is_some(), ..options };
        let work_dir = paths.temp_path.clone().unwrap_or_else(std::env::temp_dir);
        Engine {
            log: NagiosLog::new(paths.log_file.clone(), paths.log_archive_path.clone(), rotation, now),
            perf: make_perf(&paths, now),
            helper: Arc::new(CheckHelper::new(options.check_helper, &work_dir)),
            limiter: Arc::new(Semaphore::new(permits)),
            reap_now: Arc::new(Notify::new()),
            reaper: None,
            sched: Scheduler::new(),
            shared,
            rx,
            options,
            loader,
            next_retention_save: 0,
            next_freshness: 0,
            next_host_freshness: 0,
            timeperiods: HashMap::new(),
            next_timeperiod_check: 0,
        }
    }

    /// Restore retention, write startup log lines and schedule the initial checks.
    pub fn startup(&mut self) {
        let paths = self.shared.paths();
        let now = now();
        self.log.write(now, &format!("corax {} starting... (PID={})", env!("CARGO_PKG_VERSION"), std::process::id()));
        self.log.write(now, "LOG VERSION: 2.0");
        let retain = self.shared.world.read().settings.retain_state_information;
        let retained = match (&paths.state_retention_file, retain) {
            (Some(file), true) => match std::fs::read_to_string(file) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    tracing::warn!("cannot read retention file {}: {e}", file.display());
                    None
                }
            },
            _ => None,
        };
        let initial_lines;
        {
            let mut w = self.shared.world.write();
            if let Some(text) = retained {
                let stats = retention::restore(&mut w, &text);
                tracing::info!(
                    "restored retention: {} hosts, {} services, {} comments, {} downtimes ({} skipped)",
                    stats.hosts,
                    stats.services,
                    stats.comments,
                    stats.downtimes,
                    stats.skipped
                );
            }
            self.timeperiods.clear();
            initial_lines =
                initial_state_block(&w, &mut self.timeperiods, now, "INITIAL", w.settings.log_initial_states);
            self.next_timeperiod_check = next_minute(now);
            let use_retained = w.settings.use_retained_scheduling_info;
            scheduler::schedule_initial(&mut w, &mut self.sched, now, use_retained);
            self.next_retention_save = now + w.settings.retention_update_interval.max(1) * 60;
            self.next_freshness = now + w.settings.service_freshness_check_interval.max(1);
            self.next_host_freshness = now + w.settings.host_freshness_check_interval.max(1);
            w.program.program_start = now;
        }
        for line in initial_lines {
            self.log.write(now, &line);
        }
        self.log.flush();
        self.start_reaper();
        self.shared.notify_changed();
    }

    fn start_reaper(&mut self) {
        if let Some(r) = self.reaper.take() {
            r.abort();
        }
        let paths = self.shared.paths();
        let Some(dir) = paths.check_result_path.clone() else { return };
        let (freq, max_age) = {
            let w = self.shared.world.read();
            (w.settings.check_result_reaper_frequency, w.settings.max_check_result_file_age)
        };
        let tx = self.shared.sender();
        let wake = self.reap_now.clone();
        // Poll at least every 250 ms; Nagios' 1 s reaper adds up to a second of latency.
        let period = Duration::from_secs_f64(freq.clamp(0.05, 0.25));
        self.reaper = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(period) => {}
                    _ = wake.notified() => {}
                }
                let dir = dir.clone();
                let results =
                    tokio::task::spawn_blocking(move || spool::reap(&dir, now(), max_age)).await.unwrap_or_default();
                for r in results {
                    if tx.send(Input::CheckResult(r)).is_err() {
                        return;
                    }
                }
            }
        }));
    }

    /// Main loop. Returns when a shutdown was requested.
    pub async fn run(mut self, mut control: mpsc::UnboundedReceiver<Control>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let next_due = self.sched.next_due_ms();
            let sleep_for = match next_due {
                Some(ms) => Duration::from_millis((ms - (now_f64() * 1000.0) as i64).clamp(0, 1000) as u64),
                None => Duration::from_secs(1),
            };
            tokio::select! {
                c = control.recv() => match c {
                    Some(Control::Reload) => self.reload(),
                    Some(Control::Shutdown) | None => break,
                },
                msg = self.rx.recv() => {
                    let Some(msg) = msg else { break };
                    let mut batch = vec![msg];
                    while batch.len() < MAX_BATCH {
                        match self.rx.try_recv() {
                            Ok(m) => batch.push(m),
                            Err(_) => break,
                        }
                    }
                    if self.process_batch(batch) {
                        break;
                    }
                }
                _ = tokio::time::sleep(sleep_for) => self.run_due(),
                _ = tick.tick() => {
                    if self.housekeeping() {
                        break;
                    }
                }
            }
        }
        self.shutdown().await;
    }

    /// Apply inputs. Returns `true` if a shutdown was requested.
    fn process_batch(&mut self, batch: Vec<Input>) -> bool {
        let now = now();
        let mut fx = Vec::new();
        let records;
        {
            let mut guard = self.shared.world.write();
            let mut ctx = Ctx { w: &mut guard, sched: &mut self.sched, fx: &mut fx, now };
            for input in batch {
                match input {
                    Input::CheckResult(r) => checks::process_result(&mut ctx, r),
                    Input::ExternalCommand { line, source } => {
                        let _ = commands::execute(&mut ctx, &line, source);
                        ctx.w.program.last_command_check = now;
                    }
                }
            }
            records = if self.perf.is_some() { perf_records(&guard, &fx) } else { Vec::new() };
        }
        self.shared.notify_changed();
        write_perf_records(&mut self.perf, records);
        self.apply_effects(fx)
    }
}

/// A perfdata line captured under the world lock and written after it is released.
struct PerfRecord {
    time: i64,
    host: String,
    service: Option<String>,
    perf: String,
    command: String,
    state: u8,
    state_type: u8,
}

fn perf_records(w: &World, fx: &[Effect]) -> Vec<PerfRecord> {
    fx.iter()
        .filter_map(|e| if let Effect::Perfdata(t) = e { Some(*t) } else { None })
        .map(|t| {
            let st = w.state(t);
            let (host, service) = w.target_names(t);
            PerfRecord {
                time: st.last_check,
                host,
                service,
                perf: st.perf_data.clone(),
                command: w.common(t).check_command.as_ref().map(|c| c.raw.clone()).unwrap_or_default(),
                state: st.current_state,
                state_type: st.state_type as u8,
            }
        })
        .collect()
}

fn write_perf_records(perf: &mut Option<PerfdataWriter>, records: Vec<PerfRecord>) {
    let Some(perf) = perf.as_mut() else { return };
    for r in records {
        match &r.service {
            None => perf.host(r.time, &r.host, &r.perf, &r.command, r.state, r.state_type),
            Some(s) => perf.service(r.time, &r.host, s, &r.perf, &r.command, r.state, r.state_type),
        }
    }
}

impl Engine {
    /// Execute side effects. Returns `true` if a shutdown was requested.
    fn apply_effects(&mut self, fx: Vec<Effect>) -> bool {
        let mut shutdown = false;
        let mut reload = false;
        let mut save = false;
        let (notification_timeout, event_handler_timeout) = {
            let w = self.shared.world.read();
            (w.settings.notification_timeout, w.settings.event_handler_timeout)
        };
        for e in fx {
            match e {
                Effect::Log(t, line) => self.log.write(t, &line),
                Effect::Notify { cmdline } => spawn_detached(cmdline, notification_timeout, "notification"),
                Effect::EventHandler { cmdline } => spawn_detached(cmdline, event_handler_timeout, "event handler"),
                Effect::Perfdata(_) => {}
                Effect::SaveRetention => save = true,
                Effect::Restart => reload = true,
                Effect::Shutdown => shutdown = true,
            }
        }
        self.log.flush();
        if let Some(p) = self.perf.as_mut() {
            p.flush();
        }
        if save {
            self.save_retention();
        }
        if reload {
            self.reload();
        }
        shutdown
    }

    /// Start all checks that are due now.
    fn run_due(&mut self) {
        let now_ms = (now_f64() * 1000.0) as i64;
        let now = now_ms / 1000;
        let mut jobs = Vec::new();
        let mut fx = Vec::new();
        {
            let mut guard = self.shared.world.write();
            while let Some(due) = self.sched.pop_due(&guard, now_ms) {
                let mut ctx = Ctx { w: &mut guard, sched: &mut self.sched, fx: &mut fx, now };
                let Some(cmdline) = checks::prepare_active_check(&mut ctx, due.target) else {
                    // Not runnable now (disabled, outside period, already running):
                    // keep the regular rhythm going.
                    if !ctx.w.state(due.target).is_executing
                        && ctx.w.state(due.target).sched_generation == due.generation
                    {
                        scheduler::reschedule_after_check(ctx.w, ctx.sched, due.target, now, now);
                    }
                    continue;
                };
                let timeout = match due.target {
                    Target::Host(_) => ctx.w.settings.host_check_timeout,
                    Target::Service(_) => ctx.w.settings.service_check_timeout,
                };
                let names = ctx.w.target_names(due.target);
                jobs.push((due, cmdline, timeout, names, ctx.w.epoch));
            }
            self.sched.compact(&guard);
        }
        if !fx.is_empty() {
            self.apply_effects(fx);
        }
        for (due, cmdline, timeout, (host, service), epoch) in jobs {
            let target = ResultTarget::Id { target: due.target, epoch, host, service };
            self.spawn_check(due, target, cmdline, timeout);
        }
    }

    fn spawn_check(&self, due: scheduler::Due, target: ResultTarget, cmdline: String, timeout: u64) {
        let limiter = self.limiter.clone();
        let helper = self.helper.clone();
        let tx = self.shared.sender();
        let reap_now = self.reap_now.clone();
        tokio::spawn(async move {
            let _permit = limiter.acquire_owned().await;
            let start = now_f64();
            let latency = (start - due.due_ms as f64 / 1000.0).max(0.0);
            let timeout_d = Duration::from_secs(timeout);
            let inv = exec::plan(&cmdline);
            let mut via_helper = false;
            let out = match &inv {
                Invocation::Direct(argv) => {
                    via_helper = helper::is_host_check(argv);
                    match helper.run(argv, timeout_d).await {
                        Some(o) => o,
                        None => exec::run_invocation(&inv, timeout_d, &[]).await,
                    }
                }
                Invocation::Shell(_) => exec::run_invocation(&inv, timeout_d, &[]).await,
            };
            let finish = now_f64();
            let _ = tx.send(Input::CheckResult(CheckResult {
                target,
                check_type: CheckType::Active,
                return_code: out.return_code,
                output: out.output,
                start_time: start,
                finish_time: finish,
                latency,
                exited_ok: out.exited_ok,
                timed_out: out.timed_out,
                sched_generation: Some(due.generation),
            }));
            if via_helper {
                // The host check just wrote its passive results into the spool.
                reap_now.notify_one();
            }
        });
    }

    /// Periodic tasks. Returns `true` if a shutdown was requested.
    fn housekeeping(&mut self) -> bool {
        let now = now();
        let mut fx = Vec::new();
        let rotate = self.log.rotation_due(now);
        let mut rotation_lines = Vec::new();
        let mut timeperiod_lines = Vec::new();
        {
            let mut guard = self.shared.world.write();
            let mut ctx = Ctx { w: &mut guard, sched: &mut self.sched, fx: &mut fx, now };
            downtime::tick(&mut ctx);
            let services_due = now >= self.next_freshness;
            let hosts_due = now >= self.next_host_freshness;
            if services_due {
                self.next_freshness = now + ctx.w.settings.service_freshness_check_interval.max(1);
            }
            if hosts_due {
                self.next_host_freshness = now + ctx.w.settings.host_freshness_check_interval.max(1);
            }
            if services_due || hosts_due {
                check_freshness(&mut ctx, services_due, hosts_due);
            }
            ctx.w.counters.helper_checks = self.helper.served.load(std::sync::atomic::Ordering::Relaxed);
            if rotate {
                rotation_lines = initial_state_block(&guard, &mut self.timeperiods, now, "CURRENT", true);
                guard.program.last_log_rotation = now;
                self.next_timeperiod_check = next_minute(now);
            } else if now >= self.next_timeperiod_check {
                self.next_timeperiod_check = next_minute(now);
                timeperiod_lines = timeperiod_transitions(&guard, &mut self.timeperiods, now, false);
            }
        }
        for line in timeperiod_lines {
            self.log.write(now, &line);
        }
        if rotate {
            self.log.rotate(now);
            let method = self.log.rotation_method_name();
            self.log.write(now, &format!("LOG ROTATION: {method}"));
            self.log.write(now, "LOG VERSION: 2.0");
            for line in rotation_lines {
                self.log.write(now, &line);
            }
        }
        if let Some(p) = self.perf.as_mut() {
            p.maybe_rotate(now);
        }
        if now >= self.next_retention_save {
            let interval = self.shared.world.read().settings.retention_update_interval.max(1);
            self.next_retention_save = now + interval * 60;
            self.save_retention();
        }
        let changed = !fx.is_empty();
        let stop = self.apply_effects(fx);
        if changed {
            self.shared.notify_changed();
        }
        stop
    }

    fn save_retention(&self) {
        let paths = self.shared.paths();
        let Some(file) = paths.state_retention_file.clone() else { return };
        let text = {
            let w = self.shared.world.read();
            if !w.settings.retain_state_information {
                return;
            }
            retention::serialize(&w, now())
        };
        if let Err(e) = retention::save(&file, &text) {
            tracing::error!("cannot write retention file {}: {e}", file.display());
        }
    }

    /// Reload the configuration, carrying over all runtime state.
    fn reload(&mut self) {
        let now = now();
        tracing::info!("reloading configuration");
        self.log.write(now, "Caught SIGHUP, restarting...");
        let loaded = match tokio::task::block_in_place(|| (self.loader)()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("reload failed, keeping the running configuration: {e:#}");
                self.log.write(now, &format!("Error: reload failed, keeping the running configuration: {e}"));
                self.log.flush();
                return;
            }
        };
        let paths = Paths::from_main(&loaded.main);
        let mut new_world = World::new(&loaded, now);
        {
            let old = self.shared.world.read();
            let snapshot = retention::serialize(&old, now);
            retention::restore(&mut new_world, &snapshot);
            new_world.counters = old.counters.clone();
            // Program-wide switches are runtime state: always carry them over on reload.
            let mut program = old.program.clone();
            program.next_comment_id = program.next_comment_id.max(new_world.program.next_comment_id);
            program.next_downtime_id = program.next_downtime_id.max(new_world.program.next_downtime_id);
            new_world.program = program;
        }
        // Checks still running for the old world are matched by name (new epoch).
        new_world.epoch = self.shared.world.read().epoch + 1;
        let mut sched = Scheduler::new();
        let use_retained = new_world.settings.use_retained_scheduling_info;
        scheduler::schedule_initial(&mut new_world, &mut sched, now, use_retained);
        self.timeperiods.clear();
        let initial_lines = initial_state_block(
            &new_world,
            &mut self.timeperiods,
            now,
            "INITIAL",
            new_world.settings.log_initial_states,
        );
        self.next_timeperiod_check = next_minute(now);
        {
            let mut w = self.shared.world.write();
            *w = new_world;
        }
        self.sched = sched;
        self.shared.set_paths(paths.clone());
        if self.perf.is_none() {
            self.perf = make_perf(&paths, now);
        }
        self.helper.stop();
        let permits = concurrency(&self.shared.world.read(), &self.options);
        self.limiter = Arc::new(Semaphore::new(permits));
        self.start_reaper();
        self.log.write(
            now,
            &format!("corax {} reloaded configuration (PID={})", env!("CARGO_PKG_VERSION"), std::process::id()),
        );
        self.log.write(now, "LOG VERSION: 2.0");
        for line in initial_lines {
            self.log.write(now, &line);
        }
        self.log.flush();
        self.shared.notify_changed();
    }

    async fn shutdown(&mut self) {
        let now = now();
        self.log.write(now, "Caught SIGTERM, shutting down...");
        self.save_retention();
        self.helper.stop();
        if let Some(r) = self.reaper.take() {
            r.abort();
        }
        if let Some(p) = self.perf.as_mut() {
            p.flush();
        }
        self.log.write(now, &format!("Successfully shutdown... (PID={})", std::process::id()));
        self.log.flush();
    }
}

fn make_perf(paths: &Paths, now: i64) -> Option<PerfdataWriter> {
    match (&paths.npcd_perfdata_file, &paths.npcd_spool_dir) {
        (Some(f), Some(s)) => Some(PerfdataWriter::new(f.clone(), s.clone(), paths.npcd_processing_interval, now)),
        _ => None,
    }
}

fn spawn_detached(cmdline: String, timeout: u64, what: &'static str) {
    tokio::spawn(async move {
        let out = exec::run(&cmdline, Duration::from_secs(timeout), &[]).await;
        if out.timed_out {
            tracing::warn!("{what} command timed out after {timeout}s: {cmdline}");
        } else if out.return_code != 0 {
            tracing::debug!("{what} command exited with {}: {}", out.return_code, out.output.trim());
        }
    });
}

fn next_minute(now: i64) -> i64 {
    now - now.rem_euclid(60) + 60
}

/// `TIMEPERIOD TRANSITION: <name>;<from>;<to>` lines for every timeperiod whose state
/// differs from `known` (`from` is -1 for a period logged for the first time). With
/// `all`, unchanged periods are logged as well.
fn timeperiod_transitions(w: &World, known: &mut HashMap<String, bool>, now: i64, all: bool) -> Vec<String> {
    let periods = &w.objects.timeperiods;
    known.retain(|name, _| periods.contains_key(name));
    let mut names: Vec<&String> = periods.keys().collect();
    names.sort();
    let mut lines = Vec::new();
    for name in names {
        let active = timeperiod::is_active(periods, Some(name), now);
        match known.insert(name.clone(), active) {
            None => lines.push(format!("TIMEPERIOD TRANSITION: {name};-1;{}", i32::from(active))),
            Some(before) if all || before != active => {
                lines.push(format!("TIMEPERIOD TRANSITION: {name};{};{}", i32::from(before), i32::from(active)))
            }
            Some(_) => {}
        }
    }
    lines
}

/// `DOWNTIME ALERT` lines for everything that currently is in a scheduled downtime.
fn downtime_lines(w: &World) -> Vec<String> {
    let hosts = w.hosts.iter().filter(|h| h.st.scheduled_downtime_depth > 0).map(|h| {
        format!("HOST DOWNTIME ALERT: {};STARTED; Host has entered a period of scheduled downtime", h.cfg.name)
    });
    let services = w.services.iter().filter(|s| s.st.scheduled_downtime_depth > 0).map(|s| {
        format!(
            "SERVICE DOWNTIME ALERT: {};{};STARTED; Service has entered a period of scheduled downtime",
            w.host(s.host).cfg.name,
            s.cfg.description
        )
    });
    hosts.chain(services).collect()
}

/// The lines that let the `statehist` table rebuild the state of every object after a
/// start, a reload or a log rotation: the state of all timeperiods, the marker line,
/// the host and service states and the downtimes in effect.
///
/// The downtime alerts come last: `statehist` resets its downtime bookkeeping at the
/// marker line and when it sees the first state line of an object.
fn initial_state_block(
    w: &World,
    timeperiods: &mut HashMap<String, bool>,
    now: i64,
    prefix: &str,
    with_states: bool,
) -> Vec<String> {
    let mut lines = timeperiod_transitions(w, timeperiods, now, true);
    lines.push("logging initial states".to_string());
    if with_states {
        lines.extend(state_lines(w, prefix));
    }
    lines.extend(downtime_lines(w));
    lines
}

/// `INITIAL`/`CURRENT HOST STATE` lines used by availability computations.
fn state_lines(w: &World, prefix: &str) -> Vec<String> {
    let hosts = w.hosts.iter().map(|h| {
        let st = &h.st;
        format!(
            "{prefix} HOST STATE: {};{};{};{};{}",
            h.cfg.name,
            host_state_name(st.current_state),
            state_type_name(st.state_type),
            st.current_attempt,
            st.plugin_output
        )
    });
    let services = w.services.iter().map(|s| {
        let st = &s.st;
        format!(
            "{prefix} SERVICE STATE: {};{};{};{};{};{}",
            w.host(s.host).cfg.name,
            s.cfg.description,
            service_state_name(st.current_state),
            state_type_name(st.state_type),
            st.current_attempt,
            st.plugin_output
        )
    });
    hosts.chain(services).collect()
}

/// Force active checks of stale passive objects (`check_freshness`).
fn check_freshness(ctx: &mut Ctx<'_>, services: bool, hosts: bool) {
    let now = ctx.now;
    let mut stale = Vec::new();
    let check = |w: &World, t: Target, global: bool| -> Option<(i64, i64)> {
        let st = w.state(t);
        if !global || !st.check_freshness || st.is_executing || w.common(t).check_command.is_none() {
            return None;
        }
        if !timeperiod::is_active(&w.objects.timeperiods, w.common(t).check_period.as_deref(), now) {
            return None;
        }
        let configured = w.common(t).freshness_threshold;
        let threshold = if configured > 0 {
            configured
        } else {
            w.check_interval_secs(t) + st.latency as i64 + w.settings.additional_freshness_latency
        };
        let reference = if st.has_been_checked { st.last_check } else { w.program.program_start };
        let age = now - reference;
        (age > threshold).then_some((age - threshold, threshold))
    };
    let (gs, gh) = (ctx.w.program.check_service_freshness && services, ctx.w.program.check_host_freshness && hosts);
    for i in 0..ctx.w.services.len() {
        let t = Target::Service(ServiceId(i as u32));
        if let Some(x) = check(ctx.w, t, gs) {
            stale.push((t, x));
        }
    }
    for i in 0..ctx.w.hosts.len() {
        let t = Target::Host(HostId(i as u32));
        if let Some(x) = check(ctx.w, t, gh) {
            stale.push((t, x));
        }
    }
    for (t, (by, threshold)) in stale {
        let msg = match t {
            Target::Service(s) => {
                let svc = ctx.w.service(s);
                format!(
                    "Warning: The results of service '{}' on host '{}' are stale by {by} seconds (threshold={threshold} seconds).  I'm forcing an immediate check of the service.",
                    svc.cfg.description,
                    ctx.w.host(svc.host).cfg.name
                )
            }
            Target::Host(h) => format!(
                "Warning: The results of host '{}' are stale by {by} seconds (threshold={threshold} seconds).  I'm forcing an immediate check of the host.",
                ctx.w.host(h).cfg.name
            ),
        };
        ctx.log(msg);
        ctx.sched.schedule(ctx.w, t, now);
        ctx.w.state_mut(t).forced_check = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{world_from, OBJ};

    const PERIODS: &str = "define timeperiod {\n timeperiod_name 24X7\n alias Always\n sunday 00:00-24:00\n monday 00:00-24:00\n \
        tuesday 00:00-24:00\n wednesday 00:00-24:00\n thursday 00:00-24:00\n friday 00:00-24:00\n saturday 00:00-24:00\n}\n\
        define timeperiod {\n timeperiod_name never\n alias Never\n}\n";
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn timeperiod_transitions_are_logged_on_change() {
        let w = world_from(&format!("{OBJ}{PERIODS}"));
        let mut known = HashMap::new();
        assert_eq!(
            timeperiod_transitions(&w, &mut known, NOW, false),
            ["TIMEPERIOD TRANSITION: 24X7;-1;1", "TIMEPERIOD TRANSITION: never;-1;0"]
        );
        assert!(timeperiod_transitions(&w, &mut known, NOW + 60, false).is_empty());
        assert_eq!(
            timeperiod_transitions(&w, &mut known, NOW + 60, true),
            ["TIMEPERIOD TRANSITION: 24X7;1;1", "TIMEPERIOD TRANSITION: never;0;0"]
        );
        known.insert("never".to_string(), true);
        known.insert("removed".to_string(), true);
        assert_eq!(timeperiod_transitions(&w, &mut known, NOW + 120, false), ["TIMEPERIOD TRANSITION: never;1;0"]);
        assert!(!known.contains_key("removed"));
    }

    #[test]
    fn initial_state_block_ends_with_the_active_downtimes() {
        let mut w = world_from(&format!("{OBJ}{PERIODS}"));
        w.services[0].st.scheduled_downtime_depth = 1;
        let lines = initial_state_block(&w, &mut HashMap::new(), NOW, "CURRENT", true);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert_eq!(lines[1], "TIMEPERIOD TRANSITION: never;-1;0");
        assert_eq!(lines[2], "logging initial states");
        assert!(lines[3].starts_with("CURRENT HOST STATE: h1;"), "{lines:?}");
        assert!(lines[4].starts_with("CURRENT SERVICE STATE: h1;CPU load;"), "{lines:?}");
        assert_eq!(
            lines[5],
            "SERVICE DOWNTIME ALERT: h1;CPU load;STARTED; Service has entered a period of scheduled downtime"
        );
        // without `log_initial_states` only the marker and the downtimes remain
        let lines = initial_state_block(&w, &mut HashMap::new(), NOW, "INITIAL", false);
        assert_eq!(lines.len(), 4, "{lines:?}");
    }
}
