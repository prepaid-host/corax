//! External commands (`[time] NAME;arg1;arg2…`) from the command FIFO and Livestatus.
//!
//! Covers the commands issued by the Checkmk GUI, REST API and `cmk` tooling.

use crate::checkresult::{unescape_newlines, CheckResult, ResultTarget};
use crate::checks;
use crate::downtime::{self, DowntimeRequest};
use crate::effects::{Ctx, Effect};
use crate::model::*;
use crate::notify::{notify, NotifyExtra, NotifyKind};
use crate::scheduler::wants_active_checks;
use crate::CommandSource;

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub time: Option<i64>,
    pub name: String,
    pub args: Vec<String>,
    /// Everything after the name (for logging).
    pub raw_args: String,
}

/// Parse `[1700000000] NAME;a;b` (the timestamp is optional).
pub fn parse(line: &str) -> Option<ParsedCommand> {
    let line = line.trim_end_matches(['\n', '\r']).trim_start();
    let line = line.strip_prefix("COMMAND ").unwrap_or(line);
    let (time, rest) = match line.strip_prefix('[') {
        Some(r) => {
            let (ts, rest) = r.split_once(']')?;
            (ts.trim().parse::<i64>().ok(), rest.trim_start())
        }
        None => (None, line),
    };
    if rest.is_empty() {
        return None;
    }
    let (name, raw_args) = rest.split_once(';').unwrap_or((rest, ""));
    let name = name.trim().to_ascii_uppercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(ParsedCommand {
        time,
        args: if raw_args.is_empty() { Vec::new() } else { raw_args.split(';').map(String::from).collect() },
        raw_args: raw_args.to_string(),
        name,
    })
}

/// Arguments of a command with typed accessors. The *last* text argument of a command
/// (comments) may itself contain `;` and is re-joined with [`Args::rest`].
struct Args<'a> {
    v: &'a [String],
}

impl<'a> Args<'a> {
    fn s(&self, i: usize) -> Option<&'a str> {
        self.v.get(i).map(String::as_str)
    }
    fn i(&self, i: usize) -> Option<i64> {
        self.s(i).and_then(|v| v.trim().parse().ok())
    }
    fn b(&self, i: usize) -> bool {
        self.i(i).unwrap_or(0) != 0
    }
    fn rest(&self, i: usize) -> String {
        self.v.get(i..).map(|r| r.join(";")).unwrap_or_default()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandError {
    Unknown(String),
    BadArgs(String),
    NoSuchObject(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Unknown(n) => write!(f, "unknown command '{n}'"),
            CommandError::BadArgs(n) => write!(f, "invalid arguments for '{n}'"),
            CommandError::NoSuchObject(o) => write!(f, "no such object: {o}"),
        }
    }
}

fn host(ctx: &Ctx<'_>, name: Option<&str>) -> Result<HostId, CommandError> {
    let name = name.unwrap_or_default();
    ctx.w.find_host(name).ok_or_else(|| CommandError::NoSuchObject(format!("host '{name}'")))
}

fn service(ctx: &Ctx<'_>, h: Option<&str>, s: Option<&str>) -> Result<ServiceId, CommandError> {
    let (h, s) = (h.unwrap_or_default(), s.unwrap_or_default());
    ctx.w.find_service(h, s).ok_or_else(|| CommandError::NoSuchObject(format!("service '{s}' on host '{h}'")))
}

fn set_flag(ctx: &mut Ctx<'_>, t: Target, attr: u32, value: bool) {
    let st = ctx.w.state_mut(t);
    let field = match attr {
        modattr::NOTIFICATIONS_ENABLED => &mut st.notifications_enabled,
        modattr::ACTIVE_CHECKS_ENABLED => &mut st.active_checks_enabled,
        modattr::PASSIVE_CHECKS_ENABLED => &mut st.passive_checks_enabled,
        modattr::EVENT_HANDLER_ENABLED => &mut st.event_handler_enabled,
        modattr::FLAP_DETECTION_ENABLED => &mut st.flap_detection_enabled,
        modattr::PERFORMANCE_DATA_ENABLED => &mut st.process_performance_data,
        modattr::OBSESSIVE_HANDLER_ENABLED => &mut st.obsess,
        modattr::FRESHNESS_CHECKS_ENABLED => &mut st.check_freshness,
        _ => return,
    };
    if *field != value {
        *field = value;
        st.modified_attributes |= attr;
    }
    if attr == modattr::ACTIVE_CHECKS_ENABLED {
        if value {
            if wants_active_checks(ctx.w, t) && ctx.w.state(t).next_check == 0 {
                let now = ctx.now;
                ctx.sched.schedule(ctx.w, t, now);
            }
        } else {
            ctx.sched.unschedule(ctx.w, t);
        }
    }
    if attr == modattr::FLAP_DETECTION_ENABLED && !value && ctx.w.state(t).is_flapping {
        ctx.w.state_mut(t).is_flapping = false;
        downtime::delete_comments_of_type(ctx, t, comment_entry::FLAPPING);
        notify(ctx, t, NotifyKind::FlappingDisabled, NotifyExtra::default());
    }
}

fn schedule_check(ctx: &mut Ctx<'_>, t: Target, at: i64, forced: bool) {
    let st = ctx.w.state(t);
    if !forced && st.next_check > 0 && st.next_check < at {
        return; // a regular check is already due earlier
    }
    if !forced && !wants_active_checks(ctx.w, t) {
        return;
    }
    if ctx.w.common(t).check_command.is_none() {
        return;
    }
    // Forced checks of objects with active checks disabled are still executed once.
    ctx.sched.schedule(ctx.w, t, at.max(0));
    ctx.w.state_mut(t).forced_check = forced;
}

fn global_flag(ctx: &mut Ctx<'_>, name: &str) -> bool {
    let p = &mut ctx.w.program;
    let (field, value): (&mut bool, bool) = match name {
        "ENABLE_NOTIFICATIONS" => (&mut p.enable_notifications, true),
        "DISABLE_NOTIFICATIONS" => (&mut p.enable_notifications, false),
        "START_EXECUTING_SVC_CHECKS" => (&mut p.execute_service_checks, true),
        "STOP_EXECUTING_SVC_CHECKS" => (&mut p.execute_service_checks, false),
        "START_EXECUTING_HOST_CHECKS" => (&mut p.execute_host_checks, true),
        "STOP_EXECUTING_HOST_CHECKS" => (&mut p.execute_host_checks, false),
        "START_ACCEPTING_PASSIVE_SVC_CHECKS" => (&mut p.accept_passive_service_checks, true),
        "STOP_ACCEPTING_PASSIVE_SVC_CHECKS" => (&mut p.accept_passive_service_checks, false),
        "START_ACCEPTING_PASSIVE_HOST_CHECKS" => (&mut p.accept_passive_host_checks, true),
        "STOP_ACCEPTING_PASSIVE_HOST_CHECKS" => (&mut p.accept_passive_host_checks, false),
        "ENABLE_EVENT_HANDLERS" => (&mut p.enable_event_handlers, true),
        "DISABLE_EVENT_HANDLERS" => (&mut p.enable_event_handlers, false),
        "ENABLE_FLAP_DETECTION" => (&mut p.enable_flap_detection, true),
        "DISABLE_FLAP_DETECTION" => (&mut p.enable_flap_detection, false),
        "ENABLE_PERFORMANCE_DATA" => (&mut p.process_performance_data, true),
        "DISABLE_PERFORMANCE_DATA" => (&mut p.process_performance_data, false),
        "ENABLE_SERVICE_FRESHNESS_CHECKS" => (&mut p.check_service_freshness, true),
        "DISABLE_SERVICE_FRESHNESS_CHECKS" => (&mut p.check_service_freshness, false),
        "ENABLE_HOST_FRESHNESS_CHECKS" => (&mut p.check_host_freshness, true),
        "DISABLE_HOST_FRESHNESS_CHECKS" => (&mut p.check_host_freshness, false),
        "START_OBSESSING_OVER_SVC_CHECKS" => (&mut p.obsess_over_services, true),
        "STOP_OBSESSING_OVER_SVC_CHECKS" => (&mut p.obsess_over_services, false),
        "START_OBSESSING_OVER_HOST_CHECKS" => (&mut p.obsess_over_hosts, true),
        "STOP_OBSESSING_OVER_HOST_CHECKS" => (&mut p.obsess_over_hosts, false),
        _ => return false,
    };
    *field = value;
    // Record the change so retention keeps it across reloads and restarts.
    use modattr::*;
    let bit = if name.contains("NOTIFICATIONS") {
        NOTIFICATIONS_ENABLED
    } else if name.contains("EXECUTING") {
        ACTIVE_CHECKS_ENABLED
    } else if name.contains("PASSIVE") {
        PASSIVE_CHECKS_ENABLED
    } else if name.contains("EVENT_HANDLERS") {
        EVENT_HANDLER_ENABLED
    } else if name.contains("FLAP") {
        FLAP_DETECTION_ENABLED
    } else if name.contains("PERFORMANCE") {
        PERFORMANCE_DATA_ENABLED
    } else if name.contains("FRESHNESS") {
        FRESHNESS_CHECKS_ENABLED
    } else {
        OBSESSIVE_HANDLER_ENABLED
    };
    let (host, svc) = match (name.contains("HOST"), name.contains("SVC") || name.contains("SERVICE")) {
        (true, false) => (true, false),
        (false, true) => (false, true),
        _ => (true, true),
    };
    if host {
        p.modified_host_attributes |= bit;
    }
    if svc {
        p.modified_service_attributes |= bit;
    }
    true
}

/// Per-object flag commands: (name, attribute, value, applies to services of host).
fn object_flag(name: &str) -> Option<(bool, u32, bool, bool)> {
    // (is_host, attr, value, host_services)
    use modattr::*;
    Some(match name {
        "ENABLE_HOST_NOTIFICATIONS" => (true, NOTIFICATIONS_ENABLED, true, false),
        "DISABLE_HOST_NOTIFICATIONS" => (true, NOTIFICATIONS_ENABLED, false, false),
        "ENABLE_SVC_NOTIFICATIONS" => (false, NOTIFICATIONS_ENABLED, true, false),
        "DISABLE_SVC_NOTIFICATIONS" => (false, NOTIFICATIONS_ENABLED, false, false),
        "ENABLE_HOST_SVC_NOTIFICATIONS" => (true, NOTIFICATIONS_ENABLED, true, true),
        "DISABLE_HOST_SVC_NOTIFICATIONS" => (true, NOTIFICATIONS_ENABLED, false, true),
        "ENABLE_HOST_CHECK" => (true, ACTIVE_CHECKS_ENABLED, true, false),
        "DISABLE_HOST_CHECK" => (true, ACTIVE_CHECKS_ENABLED, false, false),
        "ENABLE_SVC_CHECK" => (false, ACTIVE_CHECKS_ENABLED, true, false),
        "DISABLE_SVC_CHECK" => (false, ACTIVE_CHECKS_ENABLED, false, false),
        "ENABLE_HOST_SVC_CHECKS" => (true, ACTIVE_CHECKS_ENABLED, true, true),
        "DISABLE_HOST_SVC_CHECKS" => (true, ACTIVE_CHECKS_ENABLED, false, true),
        "ENABLE_PASSIVE_HOST_CHECKS" => (true, PASSIVE_CHECKS_ENABLED, true, false),
        "DISABLE_PASSIVE_HOST_CHECKS" => (true, PASSIVE_CHECKS_ENABLED, false, false),
        "ENABLE_PASSIVE_SVC_CHECKS" => (false, PASSIVE_CHECKS_ENABLED, true, false),
        "DISABLE_PASSIVE_SVC_CHECKS" => (false, PASSIVE_CHECKS_ENABLED, false, false),
        "ENABLE_HOST_EVENT_HANDLER" => (true, EVENT_HANDLER_ENABLED, true, false),
        "DISABLE_HOST_EVENT_HANDLER" => (true, EVENT_HANDLER_ENABLED, false, false),
        "ENABLE_SVC_EVENT_HANDLER" => (false, EVENT_HANDLER_ENABLED, true, false),
        "DISABLE_SVC_EVENT_HANDLER" => (false, EVENT_HANDLER_ENABLED, false, false),
        "ENABLE_HOST_FLAP_DETECTION" => (true, FLAP_DETECTION_ENABLED, true, false),
        "DISABLE_HOST_FLAP_DETECTION" => (true, FLAP_DETECTION_ENABLED, false, false),
        "ENABLE_SVC_FLAP_DETECTION" => (false, FLAP_DETECTION_ENABLED, true, false),
        "DISABLE_SVC_FLAP_DETECTION" => (false, FLAP_DETECTION_ENABLED, false, false),
        "ENABLE_PERFORMANCE_DATA_HOST" | "ENABLE_HOST_PERFORMANCE_DATA" => {
            (true, PERFORMANCE_DATA_ENABLED, true, false)
        }
        "DISABLE_HOST_PERFORMANCE_DATA" => (true, PERFORMANCE_DATA_ENABLED, false, false),
        "ENABLE_SVC_PERFORMANCE_DATA" => (false, PERFORMANCE_DATA_ENABLED, true, false),
        "DISABLE_SVC_PERFORMANCE_DATA" => (false, PERFORMANCE_DATA_ENABLED, false, false),
        "START_OBSESSING_OVER_HOST" => (true, OBSESSIVE_HANDLER_ENABLED, true, false),
        "STOP_OBSESSING_OVER_HOST" => (true, OBSESSIVE_HANDLER_ENABLED, false, false),
        "START_OBSESSING_OVER_SVC" => (false, OBSESSIVE_HANDLER_ENABLED, true, false),
        "STOP_OBSESSING_OVER_SVC" => (false, OBSESSIVE_HANDLER_ENABLED, false, false),
        "ENABLE_HOST_FRESHNESS_CHECK" => (true, FRESHNESS_CHECKS_ENABLED, true, false),
        "DISABLE_HOST_FRESHNESS_CHECK" => (true, FRESHNESS_CHECKS_ENABLED, false, false),
        "ENABLE_SVC_FRESHNESS_CHECK" => (false, FRESHNESS_CHECKS_ENABLED, true, false),
        "DISABLE_SVC_FRESHNESS_CHECK" => (false, FRESHNESS_CHECKS_ENABLED, false, false),
        _ => return None,
    })
}

/// Execute a command line. Logs `EXTERNAL COMMAND:` like Nagios.
pub fn execute(ctx: &mut Ctx<'_>, line: &str, source: CommandSource) -> Result<(), CommandError> {
    let Some(cmd) = parse(line) else {
        ctx.log(format!("Warning: Malformed external command '{}'", line.trim()));
        return Err(CommandError::BadArgs(line.to_string()));
    };
    ctx.w.counters.external_commands += 1;
    // Passive check results are logged as PASSIVE ... CHECK instead.
    let is_result = cmd.name.starts_with("PROCESS_");
    if ctx.w.settings.log_external_commands && !is_result && cmd.name != "LOG" {
        ctx.log(format!("EXTERNAL COMMAND: {};{}", cmd.name, cmd.raw_args));
    }
    let r = dispatch(ctx, &cmd);
    if let Err(e) = &r {
        tracing::debug!("external command from {source:?} failed: {e}");
        if matches!(e, CommandError::Unknown(_)) {
            ctx.log(format!("Warning: Unrecognized external command -> {};{}", cmd.name, cmd.raw_args));
        }
    }
    r
}

fn dispatch(ctx: &mut Ctx<'_>, cmd: &ParsedCommand) -> Result<(), CommandError> {
    let a = Args { v: &cmd.args };
    let name = cmd.name.as_str();
    let bad = || CommandError::BadArgs(name.to_string());
    let now = ctx.now;

    if global_flag(ctx, name) {
        return Ok(());
    }
    if let Some((is_host, attr, value, with_services)) = object_flag(name) {
        if is_host {
            let h = host(ctx, a.s(0))?;
            if with_services {
                for s in ctx.w.host(h).services.clone() {
                    set_flag(ctx, Target::Service(s), attr, value);
                }
            } else {
                set_flag(ctx, Target::Host(h), attr, value);
            }
        } else {
            let s = service(ctx, a.s(0), a.s(1))?;
            set_flag(ctx, Target::Service(s), attr, value);
        }
        return Ok(());
    }

    match name {
        "PROCESS_SERVICE_CHECK_RESULT" => {
            let (h, s) = (a.s(0).ok_or_else(bad)?, a.s(1).ok_or_else(bad)?);
            let rc = a.i(2).ok_or_else(bad)? as i32;
            let out = unescape_newlines(&a.rest(3));
            let t = cmd.time.unwrap_or(now) as f64;
            let r = CheckResult::passive(ResultTarget::Name { host: h.into(), service: Some(s.into()) }, rc, out, t);
            checks::process_result(ctx, r);
        }
        "PROCESS_HOST_CHECK_RESULT" => {
            let h = a.s(0).ok_or_else(bad)?;
            let rc = a.i(1).ok_or_else(bad)? as i32;
            let out = unescape_newlines(&a.rest(2));
            let t = cmd.time.unwrap_or(now) as f64;
            checks::process_result(
                ctx,
                CheckResult::passive(ResultTarget::Name { host: h.into(), service: None }, rc, out, t),
            );
        }
        "SCHEDULE_SVC_CHECK" | "SCHEDULE_FORCED_SVC_CHECK" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            schedule_check(ctx, Target::Service(s), a.i(2).unwrap_or(now), name.contains("FORCED"));
        }
        "SCHEDULE_HOST_CHECK" | "SCHEDULE_FORCED_HOST_CHECK" => {
            let h = host(ctx, a.s(0))?;
            schedule_check(ctx, Target::Host(h), a.i(1).unwrap_or(now), name.contains("FORCED"));
        }
        "SCHEDULE_HOST_SVC_CHECKS" | "SCHEDULE_FORCED_HOST_SVC_CHECKS" => {
            let h = host(ctx, a.s(0))?;
            let at = a.i(1).unwrap_or(now);
            for s in ctx.w.host(h).services.clone() {
                schedule_check(ctx, Target::Service(s), at, name.contains("FORCED"));
            }
        }
        "ACKNOWLEDGE_HOST_PROBLEM" | "ACKNOWLEDGE_HOST_PROBLEM_EXPIRE" => {
            let h = host(ctx, a.s(0))?;
            let expire = name.ends_with("EXPIRE");
            let comment = a.rest(if expire { 6 } else { 5 });
            let expire_time = if expire { a.i(4).unwrap_or(0) } else { 0 };
            downtime::acknowledge(
                ctx,
                Target::Host(h),
                expire_time,
                a.i(1).unwrap_or(1) as u8,
                a.b(2),
                a.b(3),
                a.s(if expire { 5 } else { 4 }).unwrap_or(""),
                &comment,
            );
        }
        "ACKNOWLEDGE_SVC_PROBLEM" | "ACKNOWLEDGE_SVC_PROBLEM_EXPIRE" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            let expire = name.ends_with("EXPIRE");
            let comment = a.rest(if expire { 7 } else { 6 });
            let expire_time = if expire { a.i(5).unwrap_or(0) } else { 0 };
            downtime::acknowledge(
                ctx,
                Target::Service(s),
                expire_time,
                a.i(2).unwrap_or(1) as u8,
                a.b(3),
                a.b(4),
                a.s(if expire { 6 } else { 5 }).unwrap_or(""),
                &comment,
            );
        }
        "REMOVE_HOST_ACKNOWLEDGEMENT" => {
            let h = host(ctx, a.s(0))?;
            downtime::remove_acknowledgement(ctx, Target::Host(h));
        }
        "REMOVE_SVC_ACKNOWLEDGEMENT" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            downtime::remove_acknowledgement(ctx, Target::Service(s));
        }
        "ADD_HOST_COMMENT" => {
            let h = host(ctx, a.s(0))?;
            downtime::add_comment(ctx, Target::Host(h), comment_entry::USER, a.s(2).unwrap_or(""), &a.rest(3), a.b(1));
        }
        "ADD_SVC_COMMENT" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            downtime::add_comment(
                ctx,
                Target::Service(s),
                comment_entry::USER,
                a.s(3).unwrap_or(""),
                &a.rest(4),
                a.b(2),
            );
        }
        "DEL_HOST_COMMENT" | "DEL_SVC_COMMENT" => {
            let id = a.i(0).ok_or_else(bad)? as u64;
            downtime::delete_comment(ctx, id);
        }
        "DEL_ALL_HOST_COMMENTS" => {
            let h = host(ctx, a.s(0))?;
            downtime::delete_all_comments(ctx, Target::Host(h));
        }
        "DEL_ALL_SVC_COMMENTS" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            downtime::delete_all_comments(ctx, Target::Service(s));
        }
        "SCHEDULE_HOST_DOWNTIME"
        | "SCHEDULE_HOST_SVC_DOWNTIME"
        | "SCHEDULE_AND_PROPAGATE_HOST_DOWNTIME"
        | "SCHEDULE_AND_PROPAGATE_TRIGGERED_HOST_DOWNTIME" => {
            let h = host(ctx, a.s(0))?;
            let req = DowntimeRequest {
                start: a.i(1).ok_or_else(bad)?,
                end: a.i(2).ok_or_else(bad)?,
                fixed: a.b(3),
                triggered_by: a.i(4).unwrap_or(0) as u64,
                duration: a.i(5).unwrap_or(0),
                author: a.s(6).unwrap_or(""),
                comment: &a.rest(7),
            };
            if name == "SCHEDULE_HOST_SVC_DOWNTIME" {
                // Nagios: only the services of the host, not the host itself.
                for s in ctx.w.host(h).services.clone() {
                    downtime::schedule(ctx, Target::Service(s), &req);
                }
                return Ok(());
            }
            let id = downtime::schedule(ctx, Target::Host(h), &req);
            if name.starts_with("SCHEDULE_AND_PROPAGATE") && id > 0 {
                let triggered = name.contains("TRIGGERED");
                let mut stack = ctx.w.host(h).children.clone();
                let mut seen = std::collections::HashSet::new();
                while let Some(c) = stack.pop() {
                    if !seen.insert(c) {
                        continue;
                    }
                    let child_req =
                        DowntimeRequest { triggered_by: if triggered { id } else { req.triggered_by }, ..req };
                    downtime::schedule(ctx, Target::Host(c), &child_req);
                    stack.extend(ctx.w.host(c).children.clone());
                }
            }
        }
        "SCHEDULE_SVC_DOWNTIME" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            let req = DowntimeRequest {
                start: a.i(2).ok_or_else(bad)?,
                end: a.i(3).ok_or_else(bad)?,
                fixed: a.b(4),
                triggered_by: a.i(5).unwrap_or(0) as u64,
                duration: a.i(6).unwrap_or(0),
                author: a.s(7).unwrap_or(""),
                comment: &a.rest(8),
            };
            downtime::schedule(ctx, Target::Service(s), &req);
        }
        "SCHEDULE_HOSTGROUP_HOST_DOWNTIME"
        | "SCHEDULE_HOSTGROUP_SVC_DOWNTIME"
        | "SCHEDULE_SERVICEGROUP_SVC_DOWNTIME"
        | "SCHEDULE_SERVICEGROUP_HOST_DOWNTIME" => {
            let group = a.s(0).ok_or_else(bad)?;
            let req = DowntimeRequest {
                start: a.i(1).ok_or_else(bad)?,
                end: a.i(2).ok_or_else(bad)?,
                fixed: a.b(3),
                triggered_by: a.i(4).unwrap_or(0) as u64,
                duration: a.i(5).unwrap_or(0),
                author: a.s(6).unwrap_or(""),
                comment: &a.rest(7),
            };
            let targets: Vec<Target> = match name {
                "SCHEDULE_HOSTGROUP_HOST_DOWNTIME" => {
                    let g = ctx
                        .w
                        .hostgroups
                        .iter()
                        .find(|g| g.cfg.name == group)
                        .ok_or_else(|| CommandError::NoSuchObject(group.into()))?;
                    g.members.iter().map(|h| Target::Host(*h)).collect()
                }
                "SCHEDULE_HOSTGROUP_SVC_DOWNTIME" => {
                    let g = ctx
                        .w
                        .hostgroups
                        .iter()
                        .find(|g| g.cfg.name == group)
                        .ok_or_else(|| CommandError::NoSuchObject(group.into()))?;
                    g.members.iter().flat_map(|h| ctx.w.host(*h).services.iter().map(|s| Target::Service(*s))).collect()
                }
                "SCHEDULE_SERVICEGROUP_SVC_DOWNTIME" => {
                    let g = ctx
                        .w
                        .servicegroups
                        .iter()
                        .find(|g| g.cfg.name == group)
                        .ok_or_else(|| CommandError::NoSuchObject(group.into()))?;
                    g.members.iter().map(|s| Target::Service(*s)).collect()
                }
                _ => {
                    let g = ctx
                        .w
                        .servicegroups
                        .iter()
                        .find(|g| g.cfg.name == group)
                        .ok_or_else(|| CommandError::NoSuchObject(group.into()))?;
                    let mut hosts: Vec<HostId> = g.members.iter().map(|s| ctx.w.service(*s).host).collect();
                    hosts.sort();
                    hosts.dedup();
                    hosts.into_iter().map(Target::Host).collect()
                }
            };
            for t in targets {
                downtime::schedule(ctx, t, &req);
            }
        }
        "DEL_HOST_DOWNTIME" | "DEL_SVC_DOWNTIME" => {
            let id = a.i(0).ok_or_else(bad)? as u64;
            downtime::stop(ctx, id, true);
        }
        "DEL_DOWNTIME_BY_HOST_NAME" => {
            let h = host(ctx, a.s(0))?;
            let svc = a.s(1).filter(|s| !s.is_empty()).map(String::from);
            let start = a.i(2).filter(|v| *v > 0);
            let comment = a.s(3).filter(|s| !s.is_empty()).map(String::from);
            downtime::delete_where(ctx, |w, d| {
                let target_ok = w.host_of(d.target) == h
                    && match (&svc, d.target) {
                        (Some(sd), Target::Service(s)) => &w.service(s).cfg.description == sd,
                        (Some(_), Target::Host(_)) => false,
                        (None, _) => true,
                    };
                target_ok && start.is_none_or(|s| d.start_time == s) && comment.as_ref().is_none_or(|c| &d.comment == c)
            });
        }
        "SEND_CUSTOM_HOST_NOTIFICATION" => {
            let h = host(ctx, a.s(0))?;
            let extra = NotifyExtra {
                options: a.i(1).unwrap_or(0) as u32,
                author: a.s(2).unwrap_or("").into(),
                comment: a.rest(3),
            };
            notify(ctx, Target::Host(h), NotifyKind::Custom, extra);
        }
        "SEND_CUSTOM_SVC_NOTIFICATION" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            let extra = NotifyExtra {
                options: a.i(2).unwrap_or(0) as u32,
                author: a.s(3).unwrap_or("").into(),
                comment: a.rest(4),
            };
            notify(ctx, Target::Service(s), NotifyKind::Custom, extra);
        }
        "DELAY_HOST_NOTIFICATION" => {
            let h = host(ctx, a.s(0))?;
            ctx.w.host_mut(h).st.next_notification = a.i(1).unwrap_or(now);
        }
        "DELAY_SVC_NOTIFICATION" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            ctx.w.service_mut(s).st.next_notification = a.i(2).unwrap_or(now);
        }
        "CHANGE_HOST_MODATTR" => {
            let h = host(ctx, a.s(0))?;
            ctx.w.host_mut(h).st.modified_attributes = a.i(1).unwrap_or(0) as u32;
        }
        "CHANGE_SVC_MODATTR" => {
            let s = service(ctx, a.s(0), a.s(1))?;
            ctx.w.service_mut(s).st.modified_attributes = a.i(2).unwrap_or(0) as u32;
        }
        "CHANGE_HOST_CHECK_COMMAND"
        | "CHANGE_SVC_CHECK_COMMAND"
        | "CHANGE_MAX_HOST_CHECK_ATTEMPTS"
        | "CHANGE_MAX_SVC_CHECK_ATTEMPTS"
        | "CHANGE_NORMAL_HOST_CHECK_INTERVAL"
        | "CHANGE_NORMAL_SVC_CHECK_INTERVAL"
        | "CHANGE_RETRY_HOST_CHECK_INTERVAL"
        | "CHANGE_RETRY_SVC_CHECK_INTERVAL"
        | "CHANGE_CUSTOM_HOST_VAR"
        | "CHANGE_CUSTOM_SVC_VAR" => {
            // Accepted for compatibility; configuration is owned by Checkmk.
            tracing::info!("ignoring configuration change command {name}");
        }
        "SAVE_STATE_INFORMATION" => ctx.fx.push(Effect::SaveRetention),
        "RESTART_PROCESS" | "RESTART_PROGRAM" => ctx.fx.push(Effect::Restart),
        "SHUTDOWN_PROCESS" | "SHUTDOWN_PROGRAM" => ctx.fx.push(Effect::Shutdown),
        "READ_STATE_INFORMATION" => {}
        "LOG" => ctx.log(a.rest(0)),
        "ENABLE_CONTACT_HOST_NOTIFICATIONS"
        | "DISABLE_CONTACT_HOST_NOTIFICATIONS"
        | "ENABLE_CONTACT_SVC_NOTIFICATIONS"
        | "DISABLE_CONTACT_SVC_NOTIFICATIONS" => {
            let cname = a.s(0).unwrap_or_default();
            let cid = *ctx.w.contact_by_name.get(cname).ok_or_else(|| CommandError::NoSuchObject(cname.into()))?;
            let enable = name.starts_with("ENABLE");
            let c = &mut ctx.w.contacts[cid.0 as usize];
            c.modified_attributes |= modattr::NOTIFICATIONS_ENABLED;
            if name.contains("HOST") {
                c.host_notifications_enabled = enable;
            } else {
                c.service_notifications_enabled = enable;
            }
        }
        _ => return Err(CommandError::Unknown(name.to_string())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use crate::testutil::{world_from, OBJ};
    use crate::World;

    fn exec(w: &mut World, line: &str, now: i64) -> (Result<(), CommandError>, Vec<Effect>) {
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w, sched: &mut sched, fx: &mut fx, now };
        let r = execute(&mut ctx, line, CommandSource::Livestatus);
        (r, fx)
    }

    #[test]
    fn parse_variants() {
        let c = parse("[1795003860] SCHEDULE_HOST_DOWNTIME;web01;1;2;1;0;0;cmkadmin;maint; with;semicolons").unwrap();
        assert_eq!(c.time, Some(1795003860));
        assert_eq!(c.name, "SCHEDULE_HOST_DOWNTIME");
        assert_eq!(c.args.len(), 10);
        assert_eq!(parse("COMMAND [1] LOG;hello").unwrap().name, "LOG");
        assert!(parse("[1] ").is_none());
        assert!(parse("[1] BAD NAME;x").is_none());
    }

    #[test]
    fn gui_commands_roundtrip() {
        let mut w = world_from(OBJ);
        let (r, fx) = exec(&mut w, "[1795003860] ADD_HOST_COMMENT;h1;0;cmkadmin;hello; world", 100);
        assert!(r.is_ok());
        assert_eq!(w.comments.values().next().unwrap().text, "hello; world");
        assert!(fx.iter().any(
            |e| matches!(e, Effect::Log(_, l) if l == "EXTERNAL COMMAND: ADD_HOST_COMMENT;h1;0;cmkadmin;hello; world")
        ));

        let (r, _) = exec(&mut w, "[100] PROCESS_SERVICE_CHECK_RESULT;h1;CPU load;2;CRIT - x\\nline2|a=1", 100);
        assert!(r.is_ok());
        let s = w.find_service("h1", "CPU load").unwrap();
        assert_eq!(w.service(s).st.current_state, 2);
        assert_eq!(w.service(s).st.long_plugin_output, "line2");

        let (r, _) = exec(&mut w, "[100] ACKNOWLEDGE_SVC_PROBLEM;h1;CPU load;2;0;1;cmkadmin;ack", 100);
        assert!(r.is_ok());
        assert!(w.service(s).st.acknowledged);

        let (r, _) = exec(&mut w, "[100] SCHEDULE_HOST_SVC_DOWNTIME;h1;50;1000;1;0;0;cmkadmin;maint", 100);
        assert!(r.is_ok());
        // services only, not the host itself (Nagios semantics)
        assert_eq!(w.downtimes.len(), 1);
        assert_eq!(w.service(s).st.scheduled_downtime_depth, 1);
        assert_eq!(w.host(w.service(s).host).st.scheduled_downtime_depth, 0);

        let (r, _) = exec(&mut w, "[100] DEL_DOWNTIME_BY_HOST_NAME;h1", 100);
        assert!(r.is_ok());
        assert!(w.downtimes.is_empty());

        let (r, _) = exec(&mut w, "[100] DISABLE_SVC_NOTIFICATIONS;h1;CPU load", 100);
        assert!(r.is_ok());
        assert!(!w.service(s).st.notifications_enabled);
        assert_eq!(w.service(s).st.modified_attributes, modattr::NOTIFICATIONS_ENABLED);

        let (r, _) = exec(&mut w, "[100] DISABLE_NOTIFICATIONS", 100);
        assert!(r.is_ok());
        assert!(!w.program.enable_notifications);

        let (r, fx) = exec(&mut w, "[100] NO_SUCH_COMMAND;x", 100);
        assert_eq!(r, Err(CommandError::Unknown("NO_SUCH_COMMAND".into())));
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l.starts_with("Warning: Unrecognized"))));
    }

    #[test]
    fn global_switches_survive_retention() {
        let mut w = world_from(OBJ);
        exec(&mut w, "[1] DISABLE_NOTIFICATIONS", 1).0.unwrap();
        exec(&mut w, "[1] DISABLE_CONTACT_SVC_NOTIFICATIONS;admin", 1).0.unwrap();
        let text = crate::retention::serialize(&w, 1);
        let mut w2 = world_from(OBJ);
        crate::retention::restore(&mut w2, &text);
        assert!(!w2.program.enable_notifications);
        assert!(!w2.contacts[0].service_notifications_enabled);
    }

    #[test]
    fn acknowledgement_expires() {
        let mut w = world_from(OBJ);
        exec(&mut w, "[1] PROCESS_SERVICE_CHECK_RESULT;h1;CPU load;2;CRIT", 1).0.unwrap();
        exec(&mut w, "[1] ACKNOWLEDGE_SVC_PROBLEM_EXPIRE;h1;CPU load;1;0;0;500;me;later", 1).0.unwrap();
        let s = w.find_service("h1", "CPU load").unwrap();
        assert!(w.service(s).st.acknowledged);
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 600 };
        downtime::tick(&mut ctx);
        assert!(!w.service(s).st.acknowledged);
    }

    #[test]
    fn propagated_triggered_downtime_starts_children() {
        let objs = format!(
            "{OBJ}define host {{\n host_name child\n parents h1\n max_check_attempts 1\n check_command ping\n}}\n"
        );
        let mut w = world_from(&objs);
        exec(&mut w, "[100] SCHEDULE_AND_PROPAGATE_TRIGGERED_HOST_DOWNTIME;h1;50;1000;1;0;0;me;x", 100).0.unwrap();
        let child = w.find_host("child").unwrap();
        assert_eq!(w.host(child).st.scheduled_downtime_depth, 1);
    }

    #[test]
    fn forced_check_during_running_check_is_kept() {
        let mut w = world_from(OBJ);
        let s = w.find_service("h1", "CPU load").unwrap();
        w.service_mut(s).st.is_executing = true;
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 10 };
        execute(&mut ctx, "[10] SCHEDULE_FORCED_SVC_CHECK;h1;CPU load;10", CommandSource::Livestatus).unwrap();
        assert!(crate::checks::prepare_active_check(&mut ctx, Target::Service(s)).is_none());
        assert!(ctx.w.service(s).st.forced_check, "forced request must survive a running check");
    }
}
