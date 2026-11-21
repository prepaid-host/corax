//! Notification decision logic (Nagios 3 semantics) and command preparation.
//!
//! Checkmk Community uses a single Nagios contact (`check-mk-notify`) whose command
//! hands everything to `cmk --notify`, which then applies Checkmk's rule based
//! notifications. corax therefore only has to decide *whether* the core would notify
//! and expand the command line with all macros.

use corax_config::{timeperiod, StateOptions};

use crate::effects::{Ctx, Effect};
use crate::macros::{expand, host_state_name, service_state_name, MacroContext, NotificationInfo};
use crate::model::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyKind {
    Problem,
    Recovery,
    Acknowledgement,
    FlappingStart,
    FlappingStop,
    FlappingDisabled,
    DowntimeStart,
    DowntimeEnd,
    DowntimeCancelled,
    Custom,
}

impl NotifyKind {
    pub fn macro_name(&self) -> &'static str {
        match self {
            NotifyKind::Problem => "PROBLEM",
            NotifyKind::Recovery => "RECOVERY",
            NotifyKind::Acknowledgement => "ACKNOWLEDGEMENT",
            NotifyKind::FlappingStart => "FLAPPINGSTART",
            NotifyKind::FlappingStop => "FLAPPINGSTOP",
            NotifyKind::FlappingDisabled => "FLAPPINGDISABLED",
            NotifyKind::DowntimeStart => "DOWNTIMESTART",
            NotifyKind::DowntimeEnd => "DOWNTIMEEND",
            NotifyKind::DowntimeCancelled => "DOWNTIMECANCELLED",
            NotifyKind::Custom => "CUSTOM",
        }
    }

    fn is_flapping(&self) -> bool {
        matches!(self, NotifyKind::FlappingStart | NotifyKind::FlappingStop | NotifyKind::FlappingDisabled)
    }

    fn is_downtime(&self) -> bool {
        matches!(self, NotifyKind::DowntimeStart | NotifyKind::DowntimeEnd | NotifyKind::DowntimeCancelled)
    }
}

/// Author/comment for acknowledgement and custom notifications.
#[derive(Debug, Clone, Default)]
pub struct NotifyExtra {
    pub author: String,
    pub comment: String,
    /// Custom notification options: 1 = broadcast, 2 = forced, 4 = increment.
    pub options: u32,
}

fn state_label(t: Target, state: u8) -> &'static str {
    match t {
        Target::Host(_) => host_state_name(state),
        Target::Service(_) => service_state_name(state),
    }
}

/// Object level viability. Returns a reason string when the notification must not be sent.
fn object_viability(ctx: &Ctx<'_>, t: Target, kind: &NotifyKind, extra: &NotifyExtra) -> Result<(), &'static str> {
    let w = &*ctx.w;
    let st = w.state(t);
    let common = w.common(t);
    let is_host = matches!(t, Target::Host(_));
    let forced = *kind == NotifyKind::Custom && extra.options & 2 != 0;
    if forced {
        return Ok(());
    }
    if !w.program.enable_notifications {
        return Err("notifications are disabled globally");
    }
    if !st.notifications_enabled {
        return Err("notifications are disabled for this object");
    }
    if !timeperiod::is_active(&w.objects.timeperiods, common.notification_period.as_deref(), ctx.now) {
        return Err("outside of the notification period");
    }
    let host = w.host(w.host_of(t));
    match kind {
        NotifyKind::Custom => Ok(()),
        NotifyKind::Acknowledgement => {
            if st.current_state == 0 {
                Err("object is not in a problem state")
            } else {
                Ok(())
            }
        }
        k if k.is_flapping() => {
            if !common.notification_options.has(StateOptions::FLAPPING) {
                return Err("flapping notifications are not enabled");
            }
            if st.scheduled_downtime_depth > 0 {
                return Err("object is in scheduled downtime");
            }
            Ok(())
        }
        k if k.is_downtime() => {
            if !common.notification_options.has(StateOptions::DOWNTIME) {
                return Err("downtime notifications are not enabled");
            }
            if !is_host && host.st.scheduled_downtime_depth > 0 {
                return Err("host is in scheduled downtime");
            }
            Ok(())
        }
        _ => {
            // PROBLEM / RECOVERY
            if w.dependency_failed(t, false, ctx.now) {
                return Err("a notification dependency failed");
            }
            if !common.notification_options.has_state(st.current_state, is_host) {
                return Err("state is not selected in notification options");
            }
            if st.scheduled_downtime_depth > 0 {
                return Err("object is in scheduled downtime");
            }
            if st.is_flapping {
                return Err("object is flapping");
            }
            if !is_host {
                if host.st.scheduled_downtime_depth > 0 {
                    return Err("host is in scheduled downtime");
                }
                if host.st.current_state != 0 {
                    return Err("host is not UP");
                }
            }
            if *kind == NotifyKind::Problem {
                if st.acknowledged {
                    return Err("problem has been acknowledged");
                }
                let delay = (common.first_notification_delay * w.settings.interval_length as f64) as i64;
                if delay > 0 && st.current_notification_number == 0 && ctx.now < st.last_hard_state_change + delay {
                    return Err("first notification delay has not passed");
                }
                if st.current_notification_number > 0 && (st.no_more_notifications || ctx.now < st.next_notification) {
                    return Err("re-notification interval has not passed");
                }
            }
            Ok(())
        }
    }
}

fn contact_viable(ctx: &Ctx<'_>, t: Target, contact: ContactId, kind: &NotifyKind) -> bool {
    let w = &*ctx.w;
    let c = w.contact(contact);
    let is_host = matches!(t, Target::Host(_));
    let (enabled, period, opts) = if is_host {
        (c.host_notifications_enabled, &c.cfg.host_notification_period, c.cfg.host_notification_options)
    } else {
        (c.service_notifications_enabled, &c.cfg.service_notification_period, c.cfg.service_notification_options)
    };
    if !enabled || !timeperiod::is_active(&w.objects.timeperiods, period.as_deref(), ctx.now) {
        return false;
    }
    match kind {
        NotifyKind::Custom => true,
        k if k.is_flapping() => opts.has(StateOptions::FLAPPING),
        k if k.is_downtime() => opts.has(StateOptions::DOWNTIME),
        NotifyKind::Recovery => opts.has(StateOptions::RECOVERY),
        _ => opts.has_state(w.state(t).current_state, is_host),
    }
}

/// Try to send a notification. Returns `true` if at least one contact was notified.
pub fn notify(ctx: &mut Ctx<'_>, t: Target, kind: NotifyKind, extra: NotifyExtra) -> bool {
    if let Err(reason) = object_viability(ctx, t, &kind, &extra) {
        if kind == NotifyKind::Problem {
            ctx.w.state_mut(t).notification_postponement_reason = reason.to_string();
        }
        tracing::debug!("notification {} for {} suppressed: {reason}", kind.macro_name(), ctx.w.target_name(t));
        return false;
    }

    // Book-keeping before expansion so $…NOTIFICATIONNUMBER$ is correct.
    let interval_secs = {
        let common = ctx.w.common(t);
        (common.notification_interval * ctx.w.settings.interval_length as f64) as i64
    };
    let increment = matches!(kind, NotifyKind::Problem) || (kind == NotifyKind::Custom && extra.options & 4 != 0);
    {
        let now = ctx.now;
        let st = ctx.w.state_mut(t);
        st.notification_postponement_reason.clear();
        if increment {
            st.current_notification_number += 1;
        }
        if kind == NotifyKind::Problem {
            st.last_notification = now;
            if interval_secs > 0 {
                st.next_notification = now + interval_secs;
                st.no_more_notifications = false;
            } else {
                st.next_notification = 0;
                st.no_more_notifications = true;
            }
        }
    }

    let contacts: Vec<ContactId> = match t {
        Target::Host(h) => ctx.w.host(h).contacts.clone(),
        Target::Service(s) => ctx.w.service(s).contacts.clone(),
    };
    let recipients: Vec<ContactId> = contacts.into_iter().filter(|c| contact_viable(ctx, t, *c, &kind)).collect();
    if recipients.is_empty() {
        return false;
    }

    let (author_name, author_alias) = ctx
        .w
        .contact_by_name
        .get(&extra.author)
        .map(|c| {
            let cfg = &ctx.w.contact(*c).cfg;
            (cfg.name.clone(), cfg.alias.clone())
        })
        .unwrap_or_else(|| (extra.author.clone(), extra.author.clone()));
    let info = NotificationInfo {
        kind: kind.macro_name().to_string(),
        author: extra.author.clone(),
        author_name,
        author_alias,
        comment: extra.comment.clone(),
        recipients: recipients.iter().map(|c| ctx.w.contact(*c).cfg.name.clone()).collect::<Vec<_>>().join(","),
        number: ctx.w.state(t).current_notification_number,
    };

    let state = ctx.w.state(t).current_state;
    let label = match &kind {
        NotifyKind::Problem | NotifyKind::Recovery => state_label(t, state).to_string(),
        other => format!("{} ({})", other.macro_name(), state_label(t, state)),
    };
    let is_host = matches!(t, Target::Host(_));
    let mut jobs = Vec::new();
    let mut log_lines = Vec::new();
    {
        let w = &*ctx.w;
        let (host, service) = match t {
            Target::Host(h) => (h, None),
            Target::Service(s) => (w.service(s).host, Some(s)),
        };
        for cid in &recipients {
            let c = w.contact(*cid);
            let cmds = if is_host { &c.cfg.host_notification_commands } else { &c.cfg.service_notification_commands };
            for call in cmds {
                let Some(cmd) = w.objects.commands.get(&call.name) else { continue };
                let base = MacroContext {
                    world: w,
                    host: Some(host),
                    service,
                    contact: Some(*cid),
                    args: &[],
                    notification: Some(&info),
                    now: ctx.now,
                };
                let args: Vec<String> = call.args.iter().map(|a| expand(&base, a)).collect();
                let mctx = MacroContext { args: &args, ..base };
                jobs.push(expand(&mctx, &cmd.line));
                let st = w.state(t);
                let line = match service {
                    None => format!(
                        "HOST NOTIFICATION: {};{};{label};{};{}",
                        c.cfg.name,
                        w.host(host).cfg.name,
                        call.raw,
                        st.plugin_output
                    ),
                    Some(s) => format!(
                        "SERVICE NOTIFICATION: {};{};{};{label};{};{}",
                        c.cfg.name,
                        w.host(host).cfg.name,
                        w.service(s).cfg.description,
                        call.raw,
                        st.plugin_output
                    ),
                };
                log_lines.push(line);
            }
        }
    }
    let log_notifications = ctx.w.settings.log_notifications;
    for line in log_lines {
        if log_notifications {
            ctx.log(line);
        }
    }
    for cmdline in jobs {
        ctx.fx.push(Effect::Notify { cmdline });
    }
    let now = ctx.now;
    for cid in recipients {
        let c = &mut ctx.w.contacts[cid.0 as usize];
        if is_host {
            c.last_host_notification = now;
        } else {
            c.last_service_notification = now;
        }
    }
    if kind == NotifyKind::Recovery {
        let st = ctx.w.state_mut(t);
        st.current_notification_number = 0;
        st.no_more_notifications = false;
        st.next_notification = 0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::Scheduler;
    use crate::testutil::{world_from, OBJ};

    #[test]
    fn problem_notification_expands_macros_and_logs() {
        let mut w = world_from(OBJ);
        let s = w.find_service("h1", "CPU load").unwrap();
        w.service_mut(s).st.current_state = 2;
        w.service_mut(s).st.plugin_output = "CRIT - load".into();
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1_700_000_000 };
        assert!(notify(&mut ctx, Target::Service(s), NotifyKind::Problem, NotifyExtra::default()));
        let cmd = fx.iter().find_map(|e| if let Effect::Notify { cmdline } = e { Some(cmdline.clone()) } else { None });
        assert_eq!(cmd.unwrap(), "NOTIFY_WHAT='PROBLEM' /bin/true");
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l == "SERVICE NOTIFICATION: admin;h1;CPU load;CRITICAL;notify;CRIT - load")));
        assert_eq!(w.service(s).st.current_notification_number, 1);
        assert!(w.service(s).st.next_notification > 0);
    }

    #[test]
    fn suppressed_when_acknowledged_or_in_downtime() {
        let mut w = world_from(OBJ);
        let s = w.find_service("h1", "CPU load").unwrap();
        w.service_mut(s).st.current_state = 2;
        w.service_mut(s).st.acknowledged = true;
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1 };
        assert!(!notify(&mut ctx, Target::Service(s), NotifyKind::Problem, NotifyExtra::default()));
        ctx.w.service_mut(s).st.acknowledged = false;
        ctx.w.host_mut(HostId(0)).st.scheduled_downtime_depth = 1;
        assert!(!notify(&mut ctx, Target::Service(s), NotifyKind::Problem, NotifyExtra::default()));
        assert!(fx.is_empty());
    }

    const DEPENDENCIES: &str = "define service {\n host_name h1\n service_description Agent\n check_command ping\n max_check_attempts 1\n}\n\
        define service {\n host_name h1\n service_description Discovery\n check_command ping\n max_check_attempts 1\n contact_groups admins\n}\n\
        define service {\n host_name h1\n service_description Inventory\n check_command ping\n max_check_attempts 1\n contact_groups admins\n}\n\
        define servicedependency {\n host_name h1\n service_description Agent\n dependent_service_description Discovery\n \
        notification_failure_criteria u,c\n execution_failure_criteria c\n}\n\
        define servicedependency {\n host_name h1\n service_description Discovery\n dependent_service_description Inventory\n \
        notification_failure_criteria p\n inherits_parent 1\n}\n\
        define host {\n host_name h2\n address 10.1.1.2\n max_check_attempts 1\n check_command ping\n contact_groups admins\n}\n\
        define hostdependency {\n host_name h1\n dependent_host_name h2\n notification_failure_criteria d,u\n}\n";

    #[test]
    fn failed_dependencies_suppress_notifications() {
        let mut w = world_from(&format!("{OBJ}{DEPENDENCIES}"));
        let agent = w.find_service("h1", "Agent").unwrap();
        let discovery = w.find_service("h1", "Discovery").unwrap();
        let inventory = w.find_service("h1", "Inventory").unwrap();
        for s in [agent, discovery, inventory] {
            w.service_mut(s).st.has_been_checked = true;
        }
        w.service_mut(discovery).st.current_state = 2;
        w.service_mut(inventory).st.current_state = 2;
        let (d, i) = (Target::Service(discovery), Target::Service(inventory));
        let now = 1_700_000_000;
        assert!(!w.dependency_failed(d, false, now) && !w.dependency_failed(i, false, now));

        // WARNING is not part of the criteria, CRITICAL is; the chain passes it on
        w.service_mut(agent).st.current_state = 1;
        assert!(!w.dependency_failed(d, false, now));
        w.service_mut(agent).st.current_state = 2;
        assert!(w.dependency_failed(d, false, now) && w.dependency_failed(d, true, now));
        assert!(w.dependency_failed(i, false, now) && !w.dependency_failed(i, true, now));
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now };
        assert!(!notify(&mut ctx, d, NotifyKind::Problem, NotifyExtra::default()));
        // custom notifications are not held back
        assert!(notify(&mut ctx, d, NotifyKind::Custom, NotifyExtra::default()));

        // a soft state of the master does not count, its last hard state does
        ctx.w.service_mut(agent).st.state_type = StateType::Soft;
        ctx.w.service_mut(agent).st.last_hard_state = 0;
        assert!(!ctx.w.dependency_failed(d, false, now));
        assert!(notify(&mut ctx, d, NotifyKind::Problem, NotifyExtra::default()));
        ctx.w.settings.soft_state_dependencies = true;
        assert!(ctx.w.dependency_failed(d, false, now));

        // `p`: fails while the master is pending
        ctx.w.service_mut(agent).st.current_state = 0;
        assert!(!ctx.w.dependency_failed(i, false, now));
        ctx.w.service_mut(discovery).st.current_state = 0;
        ctx.w.service_mut(discovery).st.has_been_checked = false;
        assert!(ctx.w.dependency_failed(i, false, now));
    }

    #[test]
    fn host_dependencies_and_execution() {
        let mut w = world_from(&format!("{OBJ}{DEPENDENCIES}"));
        let h1 = w.find_host("h1").unwrap();
        let h2 = Target::Host(w.find_host("h2").unwrap());
        let now = 1_700_000_000;
        assert!(!w.dependency_failed(h2, false, now));
        w.host_mut(h1).st.current_state = 1;
        assert!(w.dependency_failed(h2, false, now) && !w.dependency_failed(h2, true, now));

        // a failed execution dependency postpones the scheduled check, a forced one runs
        let agent = w.find_service("h1", "Agent").unwrap();
        let discovery = Target::Service(w.find_service("h1", "Discovery").unwrap());
        w.service_mut(agent).st.current_state = 2;
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now };
        assert!(crate::checks::prepare_active_check(&mut ctx, discovery).is_none());
        assert!(!ctx.w.state(discovery).is_executing);
        ctx.w.state_mut(discovery).forced_check = true;
        assert!(crate::checks::prepare_active_check(&mut ctx, discovery).is_some());
    }
}
