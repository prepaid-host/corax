//! Comments, acknowledgements and scheduled downtimes.

use chrono::{Local, TimeZone};

use crate::effects::Ctx;
use crate::model::*;
use crate::notify::{notify, NotifyExtra, NotifyKind};

fn what(t: Target) -> (&'static str, &'static str) {
    match t {
        Target::Host(_) => ("HOST", "host"),
        Target::Service(_) => ("SERVICE", "service"),
    }
}

fn fmt_local(t: i64) -> String {
    Local.timestamp_opt(t, 0).single().map(|d| d.format("%m-%d-%Y %H:%M:%S").to_string()).unwrap_or_default()
}

// ------------------------------------------------------------------ comments

/// Add a comment and return its id.
pub fn add_comment(ctx: &mut Ctx<'_>, t: Target, entry_type: u8, author: &str, text: &str, persistent: bool) -> u64 {
    add_comment_at(ctx, t, entry_type, author, text, persistent, ctx.now, 0)
}

#[allow(clippy::too_many_arguments)]
pub fn add_comment_at(
    ctx: &mut Ctx<'_>,
    t: Target,
    entry_type: u8,
    author: &str,
    text: &str,
    persistent: bool,
    entry_time: i64,
    expire_time: i64,
) -> u64 {
    let id = ctx.w.program.next_comment_id;
    ctx.w.program.next_comment_id += 1;
    ctx.w.comments.insert(
        id,
        Comment {
            id,
            target: t,
            entry_type,
            author: author.to_string(),
            text: text.to_string(),
            entry_time,
            persistent,
            source: if entry_type == comment_entry::USER { 1 } else { 0 },
            expires: expire_time > 0,
            expire_time,
        },
    );
    id
}

pub fn delete_comment(ctx: &mut Ctx<'_>, id: u64) -> bool {
    ctx.w.comments.remove(&id).is_some()
}

pub fn delete_comments_of_type(ctx: &mut Ctx<'_>, t: Target, entry_type: u8) {
    ctx.w.comments.retain(|_, c| !(c.target == t && c.entry_type == entry_type));
}

pub fn delete_all_comments(ctx: &mut Ctx<'_>, t: Target) {
    ctx.w.comments.retain(|_, c| !(c.target == t && c.entry_type == comment_entry::USER));
}

// ------------------------------------------------------------------ acknowledgements

/// `sticky`: 0/1 = normal, 2 = sticky (until OK).
#[allow(clippy::too_many_arguments)] // mirrors the ACKNOWLEDGE_* command fields
pub fn acknowledge(
    ctx: &mut Ctx<'_>,
    t: Target,
    expire_time: i64,
    sticky: u8,
    send_notification: bool,
    persistent: bool,
    author: &str,
    comment: &str,
) {
    if ctx.w.state(t).current_state == 0 {
        return;
    }
    {
        let st = ctx.w.state_mut(t);
        st.acknowledged = true;
        st.acknowledgement_expire_time = expire_time;
        st.acknowledgement_type = if sticky == 2 { 2 } else { 1 };
    }
    add_comment(ctx, t, comment_entry::ACKNOWLEDGEMENT, author, comment, persistent);
    if send_notification {
        notify(
            ctx,
            t,
            NotifyKind::Acknowledgement,
            NotifyExtra { author: author.into(), comment: comment.into(), options: 0 },
        );
    }
}

pub fn remove_acknowledgement(ctx: &mut Ctx<'_>, t: Target) {
    let st = ctx.w.state_mut(t);
    st.acknowledged = false;
    st.acknowledgement_type = 0;
    st.acknowledgement_expire_time = 0;
    // Non-persistent acknowledgement comments go away with the acknowledgement.
    ctx.w.comments.retain(|_, c| !(c.target == t && c.entry_type == comment_entry::ACKNOWLEDGEMENT && !c.persistent));
}

// ------------------------------------------------------------------ downtimes

pub struct DowntimeRequest<'a> {
    pub start: i64,
    pub end: i64,
    pub fixed: bool,
    pub triggered_by: u64,
    pub duration: i64,
    pub author: &'a str,
    pub comment: &'a str,
}

/// Schedule a downtime. Returns its id (0 if rejected).
pub fn schedule(ctx: &mut Ctx<'_>, t: Target, req: &DowntimeRequest<'_>) -> u64 {
    if req.end <= req.start || req.end <= ctx.now {
        return 0;
    }
    let id = ctx.w.program.next_downtime_id;
    ctx.w.program.next_downtime_id += 1;
    let (_, lower) = what(t);
    let text = if req.fixed {
        format!(
            "This {lower} has been scheduled for fixed downtime from {} to {}.  Notifications for the {lower} will not be sent out during that time period.",
            fmt_local(req.start),
            fmt_local(req.end)
        )
    } else {
        let (h, m) = (req.duration / 3600, (req.duration % 3600) / 60);
        format!(
            "This {lower} has been scheduled for flexible downtime starting between {} and {} and lasting for a period of {h} hours and {m} minutes.  Notifications for the {lower} will not be sent out during that time period.",
            fmt_local(req.start),
            fmt_local(req.end)
        )
    };
    let comment_id = add_comment(ctx, t, comment_entry::DOWNTIME, req.author, &text, false);
    ctx.w.downtimes.insert(
        id,
        Downtime {
            id,
            target: t,
            author: req.author.to_string(),
            comment: req.comment.to_string(),
            entry_time: ctx.now,
            start_time: req.start,
            end_time: req.end,
            fixed: req.fixed,
            duration: if req.fixed { req.end - req.start } else { req.duration.max(0) },
            triggered_by: req.triggered_by,
            is_in_effect: false,
            flex_downtime_start: 0,
            comment_id,
        },
    );
    // Fixed downtimes that already started take effect immediately.
    let parent_active = req.triggered_by != 0 && ctx.w.downtimes.get(&req.triggered_by).is_some_and(|p| p.is_in_effect);
    if (req.fixed && req.start <= ctx.now && req.triggered_by == 0) || parent_active {
        start(ctx, id);
    }
    id
}

/// Put a downtime into effect.
pub fn start(ctx: &mut Ctx<'_>, id: u64) {
    let Some(dt) = ctx.w.downtimes.get_mut(&id) else { return };
    if dt.is_in_effect {
        return;
    }
    dt.is_in_effect = true;
    if !dt.fixed {
        dt.flex_downtime_start = ctx.now;
    }
    let t = dt.target;
    let st = ctx.w.state_mut(t);
    st.scheduled_downtime_depth += 1;
    if st.scheduled_downtime_depth == 1 {
        let (upper, lower) = what(t);
        let name = ctx.w.target_name(t);
        let label = if upper == "HOST" { "Host" } else { "Service" };
        ctx.log(format!("{upper} DOWNTIME ALERT: {name};STARTED; {label} has entered a period of scheduled downtime"));
        let _ = lower;
        notify(ctx, t, NotifyKind::DowntimeStart, NotifyExtra::default());
    }
    // Start downtimes triggered by this one.
    let triggered: Vec<u64> =
        ctx.w.downtimes.values().filter(|d| d.triggered_by == id && !d.is_in_effect).map(|d| d.id).collect();
    for tid in triggered {
        start(ctx, tid);
    }
}

/// End (`cancelled == false`) or cancel a downtime and remove it.
pub fn stop(ctx: &mut Ctx<'_>, id: u64, cancelled: bool) {
    let Some(dt) = ctx.w.downtimes.remove(&id) else { return };
    ctx.w.comments.remove(&dt.comment_id);
    if dt.is_in_effect {
        let t = dt.target;
        let st = ctx.w.state_mut(t);
        st.scheduled_downtime_depth = st.scheduled_downtime_depth.saturating_sub(1);
        if st.scheduled_downtime_depth == 0 {
            let (upper, _) = what(t);
            let name = ctx.w.target_name(t);
            let label = if upper == "HOST" { "Host" } else { "Service" };
            if cancelled {
                ctx.log(format!(
                    "{upper} DOWNTIME ALERT: {name};CANCELLED; Scheduled downtime for {} has been cancelled.",
                    label.to_lowercase()
                ));
                notify(ctx, t, NotifyKind::DowntimeCancelled, NotifyExtra::default());
            } else {
                ctx.log(format!(
                    "{upper} DOWNTIME ALERT: {name};STOPPED; {label} has exited from a period of scheduled downtime"
                ));
                notify(ctx, t, NotifyKind::DowntimeEnd, NotifyExtra::default());
            }
        }
    }
    // Downtimes triggered by this one end with it.
    let triggered: Vec<u64> = ctx.w.downtimes.values().filter(|d| d.triggered_by == id).map(|d| d.id).collect();
    for tid in triggered {
        stop(ctx, tid, cancelled);
    }
}

/// Start pending flexible downtimes when an object enters a problem state.
pub fn trigger_flexible(ctx: &mut Ctx<'_>, t: Target) {
    let now = ctx.now;
    let due: Vec<u64> = ctx
        .w
        .downtimes
        .values()
        .filter(|d| {
            d.target == t
                && !d.fixed
                && !d.is_in_effect
                && d.triggered_by == 0
                && d.start_time <= now
                && now <= d.end_time
        })
        .map(|d| d.id)
        .collect();
    for id in due {
        start(ctx, id);
    }
}

/// Periodic housekeeping: start/stop downtimes, expire comments.
pub fn tick(ctx: &mut Ctx<'_>) {
    let now = ctx.now;
    let mut to_start = Vec::new();
    let mut to_stop = Vec::new();
    for d in ctx.w.downtimes.values() {
        if d.is_in_effect {
            let end = if d.fixed { d.end_time } else { d.flex_downtime_start + d.duration };
            if now >= end {
                to_stop.push(d.id);
            }
        } else if now >= d.end_time {
            // flexible window passed without trigger, or fixed downtime fully in the past
            to_stop.push(d.id);
        } else if d.start_time <= now {
            let parent_active =
                d.triggered_by != 0 && ctx.w.downtimes.get(&d.triggered_by).is_some_and(|p| p.is_in_effect);
            // Flexible downtimes also start for objects that are already in a problem state.
            let flex_due = !d.fixed && d.triggered_by == 0 && ctx.w.state(d.target).current_state != 0;
            if parent_active || flex_due || (d.fixed && d.triggered_by == 0) {
                to_start.push(d.id);
            }
        }
    }
    for id in to_start {
        start(ctx, id);
    }
    for id in to_stop {
        stop(ctx, id, false);
    }
    ctx.w.comments.retain(|_, c| !(c.expires && c.expire_time > 0 && c.expire_time <= now));
    let expired: Vec<Target> = (0..ctx.w.hosts.len() as u32)
        .map(|i| Target::Host(HostId(i)))
        .chain((0..ctx.w.services.len() as u32).map(|i| Target::Service(ServiceId(i))))
        .filter(|t| {
            let st = ctx.w.state(*t);
            st.acknowledged && st.acknowledgement_expire_time > 0 && st.acknowledgement_expire_time <= now
        })
        .collect();
    for t in expired {
        remove_acknowledgement(ctx, t);
    }
}

/// Delete all downtimes matching a predicate (used by `DEL_DOWNTIME_BY_HOST_NAME`).
pub fn delete_where(ctx: &mut Ctx<'_>, pred: impl Fn(&crate::World, &Downtime) -> bool) -> usize {
    let w = &*ctx.w;
    let ids: Vec<u64> = w.downtimes.values().filter(|d| pred(w, d)).map(|d| d.id).collect();
    let n = ids.len();
    for id in ids {
        stop(ctx, id, true);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Effect;
    use crate::scheduler::Scheduler;
    use crate::testutil::{world_from, OBJ};

    #[test]
    fn fixed_downtime_lifecycle() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let h = Target::Host(HostId(0));
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1000 };
        let req = DowntimeRequest {
            start: 900,
            end: 2000,
            fixed: true,
            triggered_by: 0,
            duration: 0,
            author: "me",
            comment: "maint",
        };
        let id = schedule(&mut ctx, h, &req);
        assert!(id > 0);
        assert_eq!(ctx.w.host(HostId(0)).st.scheduled_downtime_depth, 1);
        assert_eq!(ctx.w.comments.len(), 1);
        ctx.now = 2000;
        tick(&mut ctx);
        assert_eq!(ctx.w.host(HostId(0)).st.scheduled_downtime_depth, 0);
        assert!(ctx.w.downtimes.is_empty());
        assert!(ctx.w.comments.is_empty());
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l == "HOST DOWNTIME ALERT: h1;STARTED; Host has entered a period of scheduled downtime")));
        assert!(fx.iter().any(|e| matches!(e, Effect::Log(_, l) if l.contains("STOPPED"))));
    }

    #[test]
    fn flexible_downtime_triggers_on_problem() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let s = Target::Service(ServiceId(0));
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1000 };
        let req = DowntimeRequest {
            start: 1000,
            end: 5000,
            fixed: false,
            triggered_by: 0,
            duration: 600,
            author: "me",
            comment: "x",
        };
        let id = schedule(&mut ctx, s, &req);
        assert!(!ctx.w.downtimes[&id].is_in_effect);
        ctx.now = 1200;
        trigger_flexible(&mut ctx, s);
        assert!(ctx.w.downtimes[&id].is_in_effect);
        ctx.now = 1800;
        tick(&mut ctx);
        assert!(ctx.w.downtimes.is_empty());
    }

    #[test]
    fn acknowledgement_comment_handling() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let s = Target::Service(ServiceId(0));
        w.service_mut(ServiceId(0)).st.current_state = 2;
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1 };
        acknowledge(&mut ctx, s, 0, 2, false, false, "admin", "on it");
        assert!(ctx.w.state(s).acknowledged);
        assert_eq!(ctx.w.state(s).acknowledgement_type, 2);
        assert_eq!(ctx.w.comments.len(), 1);
        remove_acknowledgement(&mut ctx, s);
        assert!(ctx.w.comments.is_empty());
    }

    #[test]
    fn flexible_downtime_starts_for_existing_problem() {
        let mut w = world_from(OBJ);
        let mut sched = Scheduler::new();
        let mut fx = Vec::new();
        let s = Target::Service(ServiceId(0));
        w.service_mut(ServiceId(0)).st.current_state = 2;
        let mut ctx = Ctx { w: &mut w, sched: &mut sched, fx: &mut fx, now: 1000 };
        let req = DowntimeRequest {
            start: 1000,
            end: 5000,
            fixed: false,
            triggered_by: 0,
            duration: 600,
            author: "me",
            comment: "x",
        };
        let id = schedule(&mut ctx, s, &req);
        ctx.now = 1001;
        tick(&mut ctx);
        assert!(ctx.w.downtimes[&id].is_in_effect);
    }
}
