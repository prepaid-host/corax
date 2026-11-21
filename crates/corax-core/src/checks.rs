//! Check result processing: the host/service state machine.
//!
//! Implements Nagios 3 semantics for soft/hard states, attempts, state change
//! bookkeeping, acknowledgement expiry, flap detection, alert logging, notification
//! triggering, event handlers and perfdata.

use corax_config::timeperiod;

use crate::checkresult::{parse_output, CheckResult, ResultTarget};
use crate::downtime;
use crate::effects::{Ctx, Effect};
use crate::macros::{expand, host_state_name, service_state_name, state_type_name, MacroContext};
use crate::model::*;
use crate::notify::{notify, NotifyExtra, NotifyKind};
use crate::scheduler::{reschedule_after_check, wants_active_checks};

/// Map a raw result to `(state, output)`. Host states: 0 UP, 1 DOWN (UNREACHABLE is
/// determined later from the parents).
fn normalize(ctx: &Ctx<'_>, t: Target, r: &CheckResult) -> (u8, String) {
    let s = &ctx.w.settings;
    let is_host = matches!(t, Target::Host(_));
    if r.timed_out {
        return if is_host {
            (1, format!("(Host Check Timed Out after {} seconds)", s.host_check_timeout))
        } else {
            (2, format!("(Service Check Timed Out after {} seconds)", s.service_check_timeout))
        };
    }
    let output =
        if r.output.trim().is_empty() { "(No output returned from plugin)".to_string() } else { r.output.clone() };
    if r.check_type == CheckType::Passive {
        let state = r.return_code.clamp(0, 3) as u8;
        return if is_host { (state.min(2), output) } else { (state, output) };
    }
    let (rc, output) = match r.return_code {
        rc @ 0..=3 => (rc as u8, output),
        _ if !r.exited_ok && r.output.starts_with('(') => (3, r.output.clone()),
        126 => (3, "(Return code of 126 is out of bounds - plugin may not be executable)".into()),
        127 => (3, "(Return code of 127 is out of bounds - plugin may be missing)".into()),
        rc => (3, format!("(Return code of {rc} is out of bounds)")),
    };
    if is_host {
        let up = rc == 0 || (rc == 1 && !s.use_aggressive_host_checking);
        (if up { 0 } else { 1 }, output)
    } else {
        (rc, output)
    }
}

/// DOWN becomes UNREACHABLE if the host has parents and none of them is UP.
fn host_reachability(ctx: &Ctx<'_>, h: HostId, state: u8) -> u8 {
    if state == 0 {
        return 0;
    }
    let host = ctx.w.host(h);
    if !host.parents.is_empty() && host.parents.iter().all(|p| ctx.w.host(*p).st.current_state != 0) {
        2
    } else {
        1
    }
}

fn resolve(ctx: &mut Ctx<'_>, target: &ResultTarget) -> Option<Target> {
    match target {
        ResultTarget::Id { target, epoch, .. } if *epoch == ctx.w.epoch => Some(*target),
        ResultTarget::Id { host, service, .. } => match service {
            None => ctx.w.find_host(host).map(Target::Host),
            Some(s) => ctx.w.find_service(host, s).map(Target::Service),
        },
        ResultTarget::Name { host, service: None } => {
            match ctx.w.find_host(host) {
                Some(h) => Some(Target::Host(h)),
                None => {
                    ctx.log(format!("Warning:  Passive check result was received for host '{host}', but the host could not be found!"));
                    None
                }
            }
        }
        ResultTarget::Name { host, service: Some(svc) } => match ctx.w.find_service(host, svc) {
            Some(s) => Some(Target::Service(s)),
            None => {
                ctx.log(format!(
                    "Warning:  Passive check result was received for service '{svc}' on host '{host}', but the service could not be found!"
                ));
                None
            }
        },
    }
}

/// Process one check result.
pub fn process_result(ctx: &mut Ctx<'_>, r: CheckResult) {
    let Some(t) = resolve(ctx, &r.target) else { return };
    let is_host = matches!(t, Target::Host(_));

    if r.check_type == CheckType::Active {
        let st = ctx.w.state_mut(t);
        if r.sched_generation.is_some() {
            st.is_executing = false;
        }
    } else {
        let accept = if is_host {
            ctx.w.program.accept_passive_host_checks
        } else {
            ctx.w.program.accept_passive_service_checks
        };
        if !accept || !ctx.w.state(t).passive_checks_enabled {
            return;
        }
        if ctx.w.settings.log_passive_checks {
            let first = r.output.lines().next().unwrap_or("");
            let line = match t {
                Target::Host(h) => format!("PASSIVE HOST CHECK: {};{};{first}", ctx.w.host(h).cfg.name, r.return_code),
                Target::Service(_) => {
                    format!("PASSIVE SERVICE CHECK: {};{};{first}", ctx.w.target_name(t), r.return_code)
                }
            };
            ctx.log(line);
        }
    }

    let (mut state, output) = normalize(ctx, t, &r);
    if let Target::Host(h) = t {
        state = host_reachability(ctx, h, state);
    }
    match is_host {
        true => ctx.w.counters.host_checks += 1,
        false => ctx.w.counters.service_checks += 1,
    }
    apply_state(ctx, t, state, &output, &r);

    if r.check_type == CheckType::Active {
        let start = r.start_time as i64;
        reschedule_after_check(ctx.w, ctx.sched, t, start, ctx.now);
        // A forced check requested while this one was running is executed now.
        if ctx.w.state(t).forced_check {
            let now = ctx.now;
            ctx.sched.schedule(ctx.w, t, now);
        }
    }
}

fn apply_state(ctx: &mut Ctx<'_>, t: Target, state: u8, output: &str, r: &CheckResult) {
    let now = ctx.now;
    let finish = if r.finish_time > 0.0 { r.finish_time as i64 } else { now };
    let parsed = parse_output(output);
    let is_host = matches!(t, Target::Host(_));
    let host_id = ctx.w.host_of(t);
    let host_up = ctx.w.host(host_id).st.current_state == 0;
    let max_attempts = ctx.w.common(t).max_check_attempts.max(1);
    let is_volatile = match t {
        Target::Service(s) => ctx.w.service(s).cfg.is_volatile,
        Target::Host(_) => false,
    };
    let passive_host_hard =
        is_host && r.check_type == CheckType::Passive && !ctx.w.settings.passive_host_checks_are_soft;

    let (old_state, old_type, was_checked, prev_hard) = {
        let st = ctx.w.state(t);
        (st.current_state, st.state_type, st.has_been_checked, st.last_hard_state)
    };
    let state_change = state != old_state;

    let mut next_event_id = ctx.w.program.next_event_id;
    let mut next_problem_id = ctx.w.program.next_problem_id;
    let st = ctx.w.state_mut(t);
    st.last_state = old_state;
    st.current_state = state;
    st.plugin_output = parsed.short;
    st.long_plugin_output = parsed.long;
    st.perf_data = parsed.perf;
    st.last_check = finish;
    st.has_been_checked = true;
    st.check_type = r.check_type;
    st.latency = r.latency.max(0.0);
    st.execution_time = (r.finish_time - r.start_time).max(0.0);
    st.last_time_in_state[(state as usize).min(3)] = finish;
    if !was_checked && st.last_state_change == 0 {
        st.last_state_change = finish;
        st.last_hard_state_change = finish;
    }

    // --- soft/hard state machine
    let log_type;
    if state == 0 {
        log_type = if state_change { old_type } else { StateType::Hard };
        st.state_type = StateType::Hard;
        st.current_attempt = 1;
    } else if old_state == 0 {
        st.current_attempt = 1;
        st.state_type = if max_attempts <= 1 || passive_host_hard || is_volatile || (!is_host && !host_up) {
            StateType::Hard
        } else {
            StateType::Soft
        };
        log_type = st.state_type;
    } else if old_type == StateType::Soft {
        st.current_attempt = (st.current_attempt + 1).min(max_attempts);
        if st.current_attempt >= max_attempts || passive_host_hard || (!is_host && !host_up) {
            st.state_type = StateType::Hard;
        }
        log_type = st.state_type;
    } else {
        st.state_type = StateType::Hard;
        log_type = StateType::Hard;
    }

    let mut hard_change = false;
    if st.state_type == StateType::Hard {
        if state != prev_hard {
            hard_change = true;
            st.last_hard_state_change = finish;
        }
        st.last_hard_state = state;
    }
    if state_change {
        st.last_state_change = finish;
        st.last_event_id = st.current_event_id;
        st.current_event_id = next_event_id;
        next_event_id += 1;
        if old_state == 0 {
            st.current_problem_id = next_problem_id;
            next_problem_id += 1;
        } else if state == 0 {
            st.last_problem_id = st.current_problem_id;
            st.current_problem_id = 0;
        }
    }
    let soft_retry = !state_change && st.state_type == StateType::Soft && state != 0;
    let volatile_hard = is_volatile && state != 0 && st.state_type == StateType::Hard;
    let attempt = st.current_attempt;
    let state_type = st.state_type;
    let ack_cleared = state_change && st.acknowledged && (st.acknowledgement_type != 2 || state == 0);
    ctx.w.program.next_event_id = next_event_id;
    ctx.w.program.next_problem_id = next_problem_id;

    if ack_cleared {
        downtime::remove_acknowledgement(ctx, t);
    }

    // --- logging
    let retries_logged = if is_host { ctx.w.settings.log_host_retries } else { ctx.w.settings.log_service_retries };
    if state_change || hard_change || volatile_hard || (soft_retry && retries_logged) {
        let output_line = ctx.w.state(t).plugin_output.clone();
        let line = match t {
            Target::Host(h) => format!(
                "HOST ALERT: {};{};{};{attempt};{output_line}",
                ctx.w.host(h).cfg.name,
                host_state_name(state),
                state_type_name(log_type)
            ),
            Target::Service(_) => format!(
                "SERVICE ALERT: {};{};{};{attempt};{output_line}",
                ctx.w.target_name(t),
                service_state_name(state),
                state_type_name(log_type)
            ),
        };
        ctx.log(line);
    }

    // --- flexible downtimes are triggered by problems
    if state_change && state != 0 {
        downtime::trigger_flexible(ctx, t);
    }

    check_flapping(ctx, t);

    // --- event handler on any state change (soft or hard) and soft retries
    if state_change || soft_retry || hard_change {
        run_event_handler(ctx, t);
    }

    // --- notifications
    if state_type == StateType::Hard {
        if state != 0 {
            let st = ctx.w.state(t);
            let due = hard_change
                || volatile_hard
                || st.current_notification_number == 0
                || (!st.no_more_notifications && st.next_notification > 0 && now >= st.next_notification);
            if due {
                notify(ctx, t, NotifyKind::Problem, NotifyExtra::default());
            }
        } else if hard_change {
            if ctx.w.state(t).current_notification_number > 0 {
                notify(ctx, t, NotifyKind::Recovery, NotifyExtra::default());
            }
            // Nagios resets the notification state on every hard recovery, even if
            // the recovery notification itself was suppressed.
            let st = ctx.w.state_mut(t);
            st.current_notification_number = 0;
            st.no_more_notifications = false;
            st.next_notification = 0;
        }
    }

    // --- perfdata
    if ctx.w.program.process_performance_data
        && ctx.w.state(t).process_performance_data
        && !ctx.w.state(t).perf_data.is_empty()
    {
        ctx.fx.push(Effect::Perfdata(t));
    }

    // --- dependent checks
    match t {
        Target::Service(_) if state != 0 && state_change => {
            // On-demand host check if the host result is older than the cached horizon.
            let horizon = ctx.w.settings.cached_host_check_horizon;
            let h = Target::Host(host_id);
            let hs = ctx.w.state(h);
            if hs.current_state == 0
                && !hs.is_executing
                && now - hs.last_check > horizon
                && wants_active_checks(ctx.w, h)
            {
                ctx.sched.schedule(ctx.w, h, now);
            }
        }
        Target::Host(h) if state_change && state != 0 => {
            // Children may now be unreachable: check them soon.
            let children = ctx.w.host(h).children.clone();
            for c in children {
                let ct = Target::Host(c);
                if wants_active_checks(ctx.w, ct) && !ctx.w.state(ct).is_executing {
                    ctx.sched.schedule(ctx.w, ct, now);
                }
            }
        }
        _ => {}
    }
}

/// Nagios flap detection: weighted state changes over the last 21 results.
fn check_flapping(ctx: &mut Ctx<'_>, t: Target) {
    let is_host = matches!(t, Target::Host(_));
    let (low, high) = {
        let s = &ctx.w.settings;
        let c = ctx.w.common(t);
        let (glow, ghigh) = if is_host {
            (s.low_host_flap_threshold, s.high_host_flap_threshold)
        } else {
            (s.low_service_flap_threshold, s.high_service_flap_threshold)
        };
        (
            if c.low_flap_threshold > 0.0 { c.low_flap_threshold } else { glow },
            if c.high_flap_threshold > 0.0 { c.high_flap_threshold } else { ghigh },
        )
    };
    let enabled = ctx.w.program.enable_flap_detection && ctx.w.state(t).flap_detection_enabled;
    let st = ctx.w.state_mut(t);
    st.state_history[st.state_history_index] = st.current_state;
    st.state_history_index = (st.state_history_index + 1) % FLAP_HISTORY;
    st.state_history_len = (st.state_history_len + 1).min(FLAP_HISTORY);
    if !enabled || st.state_history_len < FLAP_HISTORY {
        return;
    }
    // oldest → newest
    let hist: Vec<u8> =
        (0..FLAP_HISTORY).map(|i| st.state_history[(st.state_history_index + i) % FLAP_HISTORY]).collect();
    let mut weighted = 0.0;
    for i in 0..FLAP_HISTORY - 1 {
        if hist[i] != hist[i + 1] {
            weighted += 0.8 + 0.4 * (i as f64) / ((FLAP_HISTORY - 2) as f64);
        }
    }
    let pct = weighted * 100.0 / ((FLAP_HISTORY - 1) as f64);
    st.percent_state_change = pct;
    let was = st.is_flapping;
    let start = !was && pct >= high;
    let stop = was && pct < low;
    if start {
        st.is_flapping = true;
    }
    if stop {
        st.is_flapping = false;
    }
    let what = if is_host { "Host" } else { "Service" };
    let prefix = if is_host { "HOST" } else { "SERVICE" };
    if start {
        let name = ctx.w.target_name(t);
        ctx.log(format!("{prefix} FLAPPING ALERT: {name};STARTED; {what} appears to have started flapping ({pct:.1}% change >= {high:.1}% threshold)"));
        downtime::add_comment(
            ctx,
            t,
            comment_entry::FLAPPING,
            "(Nagios Process)",
            &format!("Notifications for this {} are being suppressed because it was detected as having been flapping between different states ({pct:.1}% change >= {high:.1}% threshold).  When the {} state stabilizes and the flapping stops, notifications will be re-enabled.", what.to_lowercase(), what.to_lowercase()),
            false,
        );
        notify(ctx, t, NotifyKind::FlappingStart, NotifyExtra::default());
    } else if stop {
        let name = ctx.w.target_name(t);
        ctx.log(format!("{prefix} FLAPPING ALERT: {name};STOPPED; {what} appears to have stopped flapping ({pct:.1}% change < {low:.1}% threshold)"));
        downtime::delete_comments_of_type(ctx, t, comment_entry::FLAPPING);
        notify(ctx, t, NotifyKind::FlappingStop, NotifyExtra::default());
    }
}

fn run_event_handler(ctx: &mut Ctx<'_>, t: Target) {
    if !ctx.w.program.enable_event_handlers || !ctx.w.state(t).event_handler_enabled {
        return;
    }
    let is_host = matches!(t, Target::Host(_));
    let global = if is_host {
        ctx.w.settings.global_host_event_handler.clone()
    } else {
        ctx.w.settings.global_service_event_handler.clone()
    };
    let calls: Vec<corax_config::CommandCall> = global
        .map(|g| corax_config::CommandCall::parse(&g))
        .into_iter()
        .chain(ctx.w.common(t).event_handler.clone())
        .collect();
    for call in calls {
        let Some(line) = crate::macros::expand_command(ctx.w, t, &call, ctx.now) else { continue };
        if ctx.w.settings.log_event_handlers {
            let st = ctx.w.state(t);
            let (state, kind) = match t {
                Target::Host(_) => (host_state_name(st.current_state), "HOST"),
                Target::Service(_) => (service_state_name(st.current_state), "SERVICE"),
            };
            let entry = format!(
                "{kind} EVENT HANDLER: {};{state};{};{};{}",
                ctx.w.target_name(t),
                state_type_name(st.state_type),
                st.current_attempt,
                call.raw
            );
            ctx.log(entry);
        }
        ctx.fx.push(Effect::EventHandler { cmdline: line });
    }
}

/// Prepare an active check: returns the expanded command line, or `None` if the check
/// cannot run now (it is then rescheduled appropriately).
pub fn prepare_active_check(ctx: &mut Ctx<'_>, t: Target) -> Option<String> {
    let is_host = matches!(t, Target::Host(_));
    let global = if is_host { ctx.w.program.execute_host_checks } else { ctx.w.program.execute_service_checks };
    if ctx.w.state(t).is_executing {
        // Keep a pending forced request; it is honoured when the running check returns.
        return None;
    }
    let forced = std::mem::take(&mut ctx.w.state_mut(t).forced_check);
    let st = ctx.w.state(t);
    if !forced && (!global || !st.active_checks_enabled) {
        return None;
    }
    let period = ctx.w.common(t).check_period.clone();
    if !forced && !timeperiod::is_active(&ctx.w.objects.timeperiods, period.as_deref(), ctx.now) {
        let next =
            timeperiod::next_active(&ctx.w.objects.timeperiods, period.as_deref(), ctx.now).unwrap_or(ctx.now + 3600);
        ctx.sched.schedule(ctx.w, t, next);
        return None;
    }
    if !forced && ctx.w.dependency_failed(t, true, ctx.now) {
        let next = ctx.now + ctx.w.check_interval_secs(t).max(1);
        ctx.sched.schedule(ctx.w, t, next);
        return None;
    }
    // Hosts without a check command are never actively checked (always UP).
    let call = ctx.w.common(t).check_command.clone()?;
    let line = crate::macros::expand_command(ctx.w, t, &call, ctx.now)?;
    ctx.w.state_mut(t).is_executing = true;
    Some(line)
}

/// Expand a command for a target (used by freshness checks and similar).
pub fn expand_for(ctx: &Ctx<'_>, t: Target, text: &str) -> String {
    expand(&MacroContext::for_target(ctx.w, t, &[], ctx.now), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use crate::testutil::{world_from, OBJ};

    fn passive(host: &str, svc: &str, rc: i32, out: &str, t: f64) -> CheckResult {
        CheckResult::passive(ResultTarget::Name { host: host.into(), service: Some(svc.into()) }, rc, out.into(), t)
    }

    fn run(w: &mut World, sched: &mut Scheduler, r: CheckResult, now: i64) -> Vec<Effect> {
        let mut fx = Vec::new();
        let mut ctx = Ctx { w, sched, fx: &mut fx, now };
        process_result(&mut ctx, r);
        fx
    }

    use crate::World;

    #[test]
    fn soft_then_hard_then_recovery() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let s = w.find_service("h1", "CPU load").unwrap();
        // max_check_attempts 3
        let fx = run(&mut w, &mut sched, passive("h1", "CPU load", 2, "CRIT - high|load=9", 100.0), 100);
        let st = &w.service(s).st;
        assert_eq!((st.current_state, st.state_type, st.current_attempt), (2, StateType::Soft, 1));
        assert_eq!(st.perf_data, "load=9");
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::Log(_, l) if l == "SERVICE ALERT: h1;CPU load;CRITICAL;SOFT;1;CRIT - high")));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Notify { .. })));

        run(&mut w, &mut sched, passive("h1", "CPU load", 2, "CRIT", 160.0), 160);
        let fx = run(&mut w, &mut sched, passive("h1", "CPU load", 2, "CRIT", 220.0), 220);
        let st = &w.service(s).st;
        assert_eq!((st.state_type, st.current_attempt, st.last_hard_state), (StateType::Hard, 3, 2));
        assert_eq!(st.last_hard_state_change, 220);
        assert!(fx.iter().any(|e| matches!(e, Effect::Notify { .. })));

        let fx = run(&mut w, &mut sched, passive("h1", "CPU load", 0, "OK", 280.0), 280);
        let st = &w.service(s).st;
        assert_eq!((st.current_state, st.state_type, st.current_attempt), (0, StateType::Hard, 1));
        assert_eq!(st.current_notification_number, 0);
        assert!(fx
            .iter()
            .any(|e| matches!(e, Effect::Log(_, l) if l.starts_with("SERVICE ALERT: h1;CPU load;OK;HARD;1"))));
        assert!(fx.iter().any(|e| matches!(e, Effect::Notify { cmdline } if cmdline.contains("RECOVERY"))));
    }

    #[test]
    fn soft_recovery_is_logged_soft() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        run(&mut w, &mut sched, passive("h1", "CPU load", 1, "WARN", 100.0), 100);
        let fx = run(&mut w, &mut sched, passive("h1", "CPU load", 0, "OK", 160.0), 160);
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l == "SERVICE ALERT: h1;CPU load;OK;SOFT;1;OK")));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Notify { .. })));
    }

    #[test]
    fn unknown_service_is_warned() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let fx = run(&mut w, &mut sched, passive("h1", "Nope", 0, "OK", 1.0), 1);
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l.contains("could not be found"))));
    }

    #[test]
    fn host_down_and_flapping_detection() {
        let mut w = world_from(OBJ);
        w.program.enable_flap_detection = true;
        let mut sched = Scheduler::new();
        let h = w.find_host("h1").unwrap();
        for i in 0..30 {
            let rc = (i % 2) as i32;
            let r = CheckResult {
                target: ResultTarget::Id { target: Target::Host(h), epoch: 0, host: "h1".into(), service: None },
                check_type: CheckType::Active,
                return_code: if rc == 0 { 0 } else { 2 },
                output: "x".into(),
                start_time: i as f64,
                finish_time: i as f64,
                latency: 0.0,
                exited_ok: true,
                timed_out: false,
                sched_generation: Some(0),
            };
            run(&mut w, &mut sched, r, i);
        }
        assert!(w.host(h).st.is_flapping);
        assert!(w.host(h).st.percent_state_change > 90.0);
        assert!(w.comments.values().any(|c| c.entry_type == comment_entry::FLAPPING));
    }

    #[test]
    fn suppressed_recovery_still_resets_notification_state() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let s = w.find_service("h1", "CPU load").unwrap();
        w.service_mut(s).cfg.common.max_check_attempts = 1;
        w.service_mut(s).cfg.common.notification_interval = 0.0;
        run(&mut w, &mut sched, passive("h1", "CPU load", 2, "CRIT", 100.0), 100);
        assert_eq!(w.service(s).st.current_notification_number, 1);
        // host in downtime → recovery notification suppressed
        w.host_mut(HostId(0)).st.scheduled_downtime_depth = 1;
        run(&mut w, &mut sched, passive("h1", "CPU load", 0, "OK", 160.0), 160);
        w.host_mut(HostId(0)).st.scheduled_downtime_depth = 0;
        assert_eq!(w.service(s).st.current_notification_number, 0);
        let fx = run(&mut w, &mut sched, passive("h1", "CPU load", 2, "CRIT", 220.0), 220);
        assert!(fx.iter().any(|e| matches!(e, Effect::Notify { .. })), "new problem must notify again");
    }
}
