//! Nagios macro expansion (`$HOSTNAME$`, `$ARG1$`, `$_HOSTTAGS$`, …).

use chrono::{Local, TimeZone};

use crate::model::*;
use crate::World;

/// Extra information available while expanding notification commands.
#[derive(Debug, Clone, Default)]
pub struct NotificationInfo {
    /// PROBLEM, RECOVERY, ACKNOWLEDGEMENT, FLAPPINGSTART, DOWNTIMESTART, CUSTOM, …
    pub kind: String,
    pub author: String,
    pub author_name: String,
    pub author_alias: String,
    pub comment: String,
    pub recipients: String,
    pub number: u32,
}

/// Everything a macro may refer to.
pub struct MacroContext<'a> {
    pub world: &'a World,
    pub host: Option<HostId>,
    pub service: Option<ServiceId>,
    pub contact: Option<ContactId>,
    pub args: &'a [String],
    pub notification: Option<&'a NotificationInfo>,
    pub now: i64,
}

impl<'a> MacroContext<'a> {
    pub fn for_target(world: &'a World, target: Target, args: &'a [String], now: i64) -> Self {
        let (host, service) = match target {
            Target::Host(h) => (Some(h), None),
            Target::Service(s) => (Some(world.service(s).host), Some(s)),
        };
        MacroContext { world, host, service, contact: None, args, notification: None, now }
    }
}

pub fn host_state_name(state: u8) -> &'static str {
    match state {
        0 => "UP",
        1 => "DOWN",
        _ => "UNREACHABLE",
    }
}

pub fn service_state_name(state: u8) -> &'static str {
    match state {
        0 => "OK",
        1 => "WARNING",
        2 => "CRITICAL",
        _ => "UNKNOWN",
    }
}

pub fn state_type_name(t: StateType) -> &'static str {
    match t {
        StateType::Soft => "SOFT",
        StateType::Hard => "HARD",
    }
}

fn fmt_time(t: i64, fmt: &str) -> String {
    Local.timestamp_opt(t, 0).single().map(|d| d.format(fmt).to_string()).unwrap_or_default()
}

/// Remove characters listed in `illegal_macro_output_chars` from output macros and from
/// all user supplied text (acknowledgement/notification comments and authors), which
/// end up inside shell command lines such as Checkmk's `check-mk-notify`.
fn clean_output(world: &World, s: &str) -> String {
    let illegal = &world.settings.illegal_macro_output_chars;
    if illegal.is_empty() {
        return s.to_string();
    }
    s.chars().filter(|c| !illegal.contains(*c)).collect()
}

fn ack_comment(world: &World, t: Target) -> Option<&Comment> {
    world.comments_of(t).filter(|c| c.entry_type == comment_entry::ACKNOWLEDGEMENT).max_by_key(|c| c.id)
}

/// Resolve one macro name (without the surrounding `$`). Returns `None` if unknown.
pub fn lookup(ctx: &MacroContext<'_>, name: &str) -> Option<String> {
    let w = ctx.world;
    if let Some(n) = name.strip_prefix("ARG") {
        let i: usize = n.parse().ok()?;
        return Some(ctx.args.get(i.checked_sub(1)?).cloned().unwrap_or_default());
    }
    if name.starts_with("USER") {
        return Some(w.resources.get(name).cloned().unwrap_or_default());
    }
    // custom variables: $_HOSTFOO$, $_SERVICEFOO$, $_CONTACTFOO$
    if let Some(var) = name.strip_prefix("_HOST") {
        let h = w.host(ctx.host?);
        return Some(h.cfg.common.custom_var(var).unwrap_or_default().to_string());
    }
    if let Some(var) = name.strip_prefix("_SERVICE") {
        let s = w.service(ctx.service?);
        return Some(s.cfg.common.custom_var(var).unwrap_or_default().to_string());
    }
    if let Some(var) = name.strip_prefix("_CONTACT") {
        let c = w.contact(ctx.contact?);
        return Some(c.cfg.custom_vars.iter().find(|(k, _)| k == var).map(|(_, v)| v.clone()).unwrap_or_default());
    }

    let now = ctx.now;
    match name {
        "TIMET" => return Some(now.to_string()),
        "DATE" => return Some(fmt_time(now, "%Y-%m-%d")),
        "TIME" => return Some(fmt_time(now, "%H:%M:%S")),
        "SHORTDATETIME" => return Some(fmt_time(now, "%Y-%m-%d %H:%M:%S")),
        "LONGDATETIME" => return Some(fmt_time(now, "%a %b %d %H:%M:%S %Z %Y")),
        "PROCESSSTARTTIME" => return Some(w.program.program_start.to_string()),
        "TOTALHOSTSUP" => return Some(w.hosts.iter().filter(|h| h.st.current_state == 0).count().to_string()),
        "TOTALSERVICESOK" => return Some(w.services.iter().filter(|s| s.st.current_state == 0).count().to_string()),
        _ => {}
    }

    if let Some(n) = ctx.notification {
        match name {
            "NOTIFICATIONTYPE" => return Some(n.kind.clone()),
            "NOTIFICATIONCOMMENT" | "HOSTNOTIFICATIONCOMMENT" | "SERVICENOTIFICATIONCOMMENT" => {
                return Some(clean_output(w, &n.comment))
            }
            "NOTIFICATIONAUTHOR" => return Some(clean_output(w, &n.author)),
            "NOTIFICATIONAUTHORNAME" => return Some(clean_output(w, &n.author_name)),
            "NOTIFICATIONAUTHORALIAS" => return Some(clean_output(w, &n.author_alias)),
            "NOTIFICATIONRECIPIENTS" => return Some(n.recipients.clone()),
            "NOTIFICATIONNUMBER" => return Some(n.number.to_string()),
            _ => {}
        }
    } else if matches!(
        name,
        "NOTIFICATIONTYPE"
            | "NOTIFICATIONCOMMENT"
            | "NOTIFICATIONAUTHOR"
            | "NOTIFICATIONAUTHORNAME"
            | "NOTIFICATIONAUTHORALIAS"
            | "NOTIFICATIONRECIPIENTS"
    ) {
        return Some(String::new());
    }

    if let Some(cid) = ctx.contact {
        let c = &w.contact(cid).cfg;
        match name {
            "CONTACTNAME" => return Some(c.name.clone()),
            "CONTACTALIAS" => return Some(c.alias.clone()),
            "CONTACTEMAIL" => return Some(c.email.clone()),
            "CONTACTPAGER" => return Some(c.pager.clone()),
            "CONTACTGROUPNAMES" => return Some(c.contactgroups.join(",")),
            _ => {
                if let Some(n) = name.strip_prefix("CONTACTADDRESS") {
                    let i: usize = n.parse().ok()?;
                    return Some(c.addresses.get(i.checked_sub(1)?).cloned().unwrap_or_default());
                }
            }
        }
    }

    if let Some(hid) = ctx.host {
        let h = w.host(hid);
        let st = &h.st;
        let t = Target::Host(hid);
        let v = match name {
            "HOSTNAME" => Some(h.cfg.name.clone()),
            "HOSTALIAS" => Some(h.cfg.alias.clone()),
            "HOSTADDRESS" => Some(h.cfg.address.clone()),
            "HOSTDISPLAYNAME" => Some(h.cfg.common.display_name.clone()),
            "HOSTSTATE" => Some(host_state_name(st.current_state).into()),
            "HOSTSTATEID" => Some(st.current_state.to_string()),
            "LASTHOSTSTATE" => Some(host_state_name(st.last_state).into()),
            "LASTHOSTSTATEID" => Some(st.last_state.to_string()),
            "HOSTSTATETYPE" => Some(state_type_name(st.state_type).into()),
            "HOSTATTEMPT" => Some(st.current_attempt.to_string()),
            "MAXHOSTATTEMPTS" => Some(h.cfg.common.max_check_attempts.to_string()),
            "HOSTOUTPUT" => Some(clean_output(w, &st.plugin_output)),
            "LONGHOSTOUTPUT" => Some(clean_output(w, &st.long_plugin_output).replace('\n', "\\n")),
            "HOSTPERFDATA" => Some(clean_output(w, &st.perf_data)),
            "HOSTCHECKCOMMAND" => Some(h.cfg.common.check_command.as_ref().map(|c| c.raw.clone()).unwrap_or_default()),
            "HOSTLATENCY" => Some(format!("{:.3}", st.latency)),
            "HOSTEXECUTIONTIME" => Some(format!("{:.3}", st.execution_time)),
            "HOSTDURATIONSEC" => Some((now - st.last_state_change).max(0).to_string()),
            "LASTHOSTCHECK" => Some(st.last_check.to_string()),
            "LASTHOSTSTATECHANGE" => Some(st.last_state_change.to_string()),
            "LASTHOSTUP" => Some(st.last_time_in_state[0].to_string()),
            "LASTHOSTDOWN" => Some(st.last_time_in_state[1].to_string()),
            "LASTHOSTUNREACHABLE" => Some(st.last_time_in_state[2].to_string()),
            "HOSTDOWNTIME" => Some(st.scheduled_downtime_depth.to_string()),
            "HOSTPERCENTCHANGE" => Some(format!("{:.2}", st.percent_state_change)),
            "HOSTGROUPNAMES" => Some(h.cfg.hostgroups.join(",")),
            "HOSTGROUPNAME" => Some(h.cfg.hostgroups.first().cloned().unwrap_or_default()),
            "HOSTNOTIFICATIONNUMBER" => Some(st.current_notification_number.to_string()),
            "HOSTPROBLEMID" => Some(st.current_problem_id.to_string()),
            "LASTHOSTPROBLEMID" => Some(st.last_problem_id.to_string()),
            "HOSTEVENTID" => Some(st.current_event_id.to_string()),
            "LASTHOSTEVENTID" => Some(st.last_event_id.to_string()),
            "HOSTACKAUTHOR" | "HOSTACKAUTHORNAME" | "HOSTACKAUTHORALIAS" => {
                Some(clean_output(w, &ack_comment(w, t).map(|c| c.author.clone()).unwrap_or_default()))
            }
            "HOSTACKCOMMENT" => Some(clean_output(w, &ack_comment(w, t).map(|c| c.text.clone()).unwrap_or_default())),
            "HOSTNOTES" => Some(h.cfg.common.notes.clone()),
            "HOSTNOTESURL" => Some(h.cfg.common.notes_url.clone()),
            "HOSTACTIONURL" => Some(h.cfg.common.action_url.clone()),
            "HOSTCHECKTYPE" => Some(if st.check_type == CheckType::Active { "ACTIVE" } else { "PASSIVE" }.into()),
            _ => None,
        };
        if v.is_some() {
            return v;
        }
    }

    if let Some(sid) = ctx.service {
        let s = w.service(sid);
        let st = &s.st;
        let t = Target::Service(sid);
        let v = match name {
            "SERVICEDESC" => Some(s.cfg.description.clone()),
            "SERVICEDISPLAYNAME" => Some(s.cfg.common.display_name.clone()),
            "SERVICESTATE" => Some(service_state_name(st.current_state).into()),
            "SERVICESTATEID" => Some(st.current_state.to_string()),
            "LASTSERVICESTATE" => Some(service_state_name(st.last_state).into()),
            "LASTSERVICESTATEID" => Some(st.last_state.to_string()),
            "SERVICESTATETYPE" => Some(state_type_name(st.state_type).into()),
            "SERVICEATTEMPT" => Some(st.current_attempt.to_string()),
            "MAXSERVICEATTEMPTS" => Some(s.cfg.common.max_check_attempts.to_string()),
            "SERVICEISVOLATILE" => Some((s.cfg.is_volatile as u8).to_string()),
            "SERVICEOUTPUT" => Some(clean_output(w, &st.plugin_output)),
            "LONGSERVICEOUTPUT" => Some(clean_output(w, &st.long_plugin_output).replace('\n', "\\n")),
            "SERVICEPERFDATA" => Some(clean_output(w, &st.perf_data)),
            "SERVICECHECKCOMMAND" => {
                Some(s.cfg.common.check_command.as_ref().map(|c| c.raw.clone()).unwrap_or_default())
            }
            "SERVICELATENCY" => Some(format!("{:.3}", st.latency)),
            "SERVICEEXECUTIONTIME" => Some(format!("{:.3}", st.execution_time)),
            "SERVICEDURATIONSEC" => Some((now - st.last_state_change).max(0).to_string()),
            "LASTSERVICECHECK" => Some(st.last_check.to_string()),
            "LASTSERVICESTATECHANGE" => Some(st.last_state_change.to_string()),
            "LASTSERVICEOK" => Some(st.last_time_in_state[0].to_string()),
            "LASTSERVICEWARNING" => Some(st.last_time_in_state[1].to_string()),
            "LASTSERVICECRITICAL" => Some(st.last_time_in_state[2].to_string()),
            "LASTSERVICEUNKNOWN" => Some(st.last_time_in_state[3].to_string()),
            "SERVICEDOWNTIME" => Some(st.scheduled_downtime_depth.to_string()),
            "SERVICEPERCENTCHANGE" => Some(format!("{:.2}", st.percent_state_change)),
            "SERVICEGROUPNAMES" => Some(s.cfg.servicegroups.join(",")),
            "SERVICEGROUPNAME" => Some(s.cfg.servicegroups.first().cloned().unwrap_or_default()),
            "SERVICENOTIFICATIONNUMBER" => Some(st.current_notification_number.to_string()),
            "SERVICEPROBLEMID" => Some(st.current_problem_id.to_string()),
            "LASTSERVICEPROBLEMID" => Some(st.last_problem_id.to_string()),
            "SERVICEEVENTID" => Some(st.current_event_id.to_string()),
            "LASTSERVICEEVENTID" => Some(st.last_event_id.to_string()),
            "SERVICEACKAUTHOR" | "SERVICEACKAUTHORNAME" | "SERVICEACKAUTHORALIAS" => {
                Some(clean_output(w, &ack_comment(w, t).map(|c| c.author.clone()).unwrap_or_default()))
            }
            "SERVICEACKCOMMENT" => {
                Some(clean_output(w, &ack_comment(w, t).map(|c| c.text.clone()).unwrap_or_default()))
            }
            "SERVICENOTES" => Some(s.cfg.common.notes.clone()),
            "SERVICENOTESURL" => Some(s.cfg.common.notes_url.clone()),
            "SERVICEACTIONURL" => Some(s.cfg.common.action_url.clone()),
            "SERVICECHECKTYPE" => Some(if st.check_type == CheckType::Active { "ACTIVE" } else { "PASSIVE" }.into()),
            _ => None,
        };
        if v.is_some() {
            return v;
        }
    } else if name.starts_with("SERVICE") || name.starts_with("LASTSERVICE") || name.starts_with("LONGSERVICE") {
        // Service macros in a host context expand to empty strings (Nagios behaviour).
        return Some(String::new());
    }
    None
}

/// Expand all `$MACRO$` occurrences. `$$` yields a literal `$`. Unknown macros are kept.
pub fn expand(ctx: &MacroContext<'_>, template: &str) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('$') {
            Some(0) => {
                out.push('$');
                rest = &after[1..];
            }
            Some(end) => {
                let name = &after[..end];
                let valid = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                match valid.then(|| lookup(ctx, name)).flatten() {
                    Some(v) => out.push_str(&v),
                    None => {
                        out.push('$');
                        out.push_str(name);
                        out.push('$');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Expand a command call: `$ARGn$` values are macro-expanded first, then the command line.
pub fn expand_command(world: &World, target: Target, call: &corax_config::CommandCall, now: i64) -> Option<String> {
    let cmd = world.objects.commands.get(&call.name)?;
    let base = MacroContext::for_target(world, target, &[], now);
    let args: Vec<String> = call.args.iter().map(|a| expand(&base, a)).collect();
    let ctx = MacroContext::for_target(world, target, &args, now);
    Some(expand(&ctx, &cmd.line))
}

/// Expand URLs and notes (Livestatus `*_expanded` columns).
pub fn expand_url(world: &World, target: Target, text: &str) -> String {
    if !text.contains('$') {
        return text.to_string();
    }
    expand(&MacroContext::for_target(world, target, &[], crate::now()), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{world_from, OBJ};

    #[test]
    fn command_expansion() {
        let w = world_from(OBJ);
        let h = w.find_host("h1").unwrap();
        let call = w.host(h).cfg.common.check_command.clone().unwrap();
        let line = expand_command(&w, Target::Host(h), &call, 0).unwrap();
        assert_eq!(line, "/usr/lib/plugins/check_icmp -w h1 10.1.1.1");
    }

    #[test]
    fn custom_and_unknown_and_dollar() {
        let w = world_from(OBJ);
        let s = w.find_service("h1", "CPU load").unwrap();
        let ctx = MacroContext::for_target(&w, Target::Service(s), &[], 0);
        assert_eq!(expand(&ctx, "$_HOSTTAGS$|$SERVICEDESC$|$NOPE$|$$|$HOSTSTATE$"), "a b|CPU load|$NOPE$|$|UP");
    }

    #[test]
    fn user_text_cannot_break_out_of_shell_quotes() {
        let mut w = world_from(OBJ);
        w.settings.illegal_macro_output_chars = "`~$&|'\"<>".into();
        let s = w.find_service("h1", "CPU load").unwrap();
        w.comments.insert(
            1,
            Comment {
                id: 1,
                target: Target::Service(s),
                entry_type: comment_entry::ACKNOWLEDGEMENT,
                author: "me'; id; '".into(),
                text: "x'; touch /tmp/pwn; '".into(),
                entry_time: 0,
                persistent: false,
                source: 0,
                expires: false,
                expire_time: 0,
            },
        );
        let info = NotificationInfo { comment: "a'b".into(), author: "c'd".into(), ..Default::default() };
        let ctx =
            MacroContext { notification: Some(&info), ..MacroContext::for_target(&w, Target::Service(s), &[], 0) };
        let out = expand(
            &ctx,
            "A='$SERVICEACKCOMMENT$' B='$SERVICEACKAUTHOR$' C='$NOTIFICATIONCOMMENT$' D='$NOTIFICATIONAUTHOR$'",
        );
        assert_eq!(out, "A='x; touch /tmp/pwn; ' B='me; id; ' C='ab' D='cd'");
    }
}
