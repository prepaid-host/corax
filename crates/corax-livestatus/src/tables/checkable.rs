//! Columns shared by hosts and services.

use corax_config::CheckableConfig;
use corax_core::model::modattr;
use corax_core::{CheckState, Host, Service, Target};

use super::Ctx;
use crate::labels;
use crate::value::Value;

/// A host or a service viewed through its common attributes.
pub struct Obj<'a> {
    pub target: Target,
    pub cfg: &'a CheckableConfig,
    pub st: &'a CheckState,
    pub host: &'a Host,
    pub svc: Option<&'a Service>,
}

impl<'a> Obj<'a> {
    pub fn host(h: &'a Host) -> Self {
        Obj { target: Target::Host(h.id), cfg: &h.cfg.common, st: &h.st, host: h, svc: None }
    }

    pub fn service(ctx: &Ctx<'a>, s: &'a Service) -> Self {
        Obj { target: Target::Service(s.id), cfg: &s.cfg.common, st: &s.st, host: ctx.world.host(s.host), svc: Some(s) }
    }
}

pub type ObjFn = for<'x, 'y> fn(&Ctx<'x>, &Obj<'y>) -> Value;

/// Define a column getter as a plain function pointer.
macro_rules! col {
    ($c:ident, $o:ident => $e:expr) => {
        Some(
            (|$c: &Ctx<'_>, $o: &Obj<'_>| -> Value {
                let _ = (&$c, &$o);
                $e
            }) as ObjFn,
        )
    };
}

fn ids<I: Iterator<Item = u64>>(it: I) -> Value {
    Value::List(it.map(|i| Value::Int(i as i64)).collect())
}

/// Parse perfdata `name=value;warn;crit;min;max` into `(name, value)` pairs.
pub fn perf_values(perf: &str) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let mut rest = perf.trim();
    while !rest.is_empty() {
        // label may be single-quoted and contain spaces
        let (label, after) = if let Some(r) = rest.strip_prefix('\'') {
            match r.find("'=") {
                Some(end) => (r[..end].to_string(), &r[end + 2..]),
                None => break,
            }
        } else {
            match rest.find('=') {
                Some(eq) if !rest[..eq].contains(char::is_whitespace) => (rest[..eq].to_string(), &rest[eq + 1..]),
                _ => {
                    // skip garbage token
                    rest = rest.split_once(char::is_whitespace).map(|x| x.1.trim_start()).unwrap_or("");
                    continue;
                }
            }
        };
        let (token, next) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
        let value_txt = token.split(';').next().unwrap_or("");
        let num: String = value_txt
            .chars()
            .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E' | ','))
            .map(|c| if c == ',' { '.' } else { c })
            .collect();
        if let Ok(v) = num.parse::<f64>() {
            out.push((label, v));
        }
        rest = next.trim_start();
    }
    out
}

/// Livestatus `pnp_cleanup`: replace ` /\:` with `_`.
pub fn pnp_cleanup(s: &str) -> String {
    s.chars().map(|c| if matches!(c, ' ' | '/' | '\\' | ':') { '_' } else { c }).collect()
}

fn escaped_long_output(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn modified_list(bits: u32) -> Value {
    Value::str_list(modattr::NAMES.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n))
}

fn pending_flex(c: &Ctx<'_>, t: Target) -> i64 {
    c.world
        .downtimes_of(t)
        .filter(|d| !d.fixed && !d.is_in_effect && d.start_time <= c.now && c.now <= d.end_time)
        .count() as i64
}

pub fn column(name: &str) -> Option<ObjFn> {
    match name {
        "accept_passive_checks" => col!(c, o => Value::bool(o.st.passive_checks_enabled)),
        "acknowledged" => col!(c, o => Value::bool(o.st.acknowledged)),
        "acknowledgement_type" => col!(c, o => Value::Int(o.st.acknowledgement_type as i64)),
        "action_url" => col!(c, o => Value::str(&o.cfg.action_url)),
        "action_url_expanded" => {
            col!(c, o => Value::Str(corax_core::macros::expand_url(c.world, o.target, &o.cfg.action_url)))
        }
        "active_checks_enabled" | "checks_enabled" => col!(c, o => Value::bool(o.st.active_checks_enabled)),
        "check_command" => {
            col!(c, o => Value::Str(o.cfg.check_command.as_ref().map(|x| x.raw.clone()).unwrap_or_default()))
        }
        "check_command_expanded" => col!(c, o => Value::Str(
            o.cfg.check_command.as_ref().map(|x| corax_core::macros::expand_url(c.world, o.target, &x.raw)).unwrap_or_default()
        )),
        "check_flapping_recovery_notification" | "check_options" | "initial_state" => col!(c, o => Value::Int(0)),
        "check_freshness" => col!(c, o => Value::bool(o.st.check_freshness)),
        "check_interval" => col!(c, o => Value::Float(o.cfg.check_interval)),
        "retry_interval" => col!(c, o => Value::Float(o.cfg.retry_interval)),
        "check_period" => col!(c, o => Value::Str(o.cfg.check_period.clone().unwrap_or_default())),
        "check_type" => col!(c, o => Value::Int(o.st.check_type as i64)),
        "comments" => col!(c, o => ids(c.world.comments_of(o.target).map(|x| x.id))),
        "comments_with_info" => col!(c, o => Value::List(
            c.world.comments_of(o.target)
                .map(|x| Value::List(vec![Value::Int(x.id as i64), Value::str(&x.author), Value::str(&x.text)]))
                .collect()
        )),
        "comments_with_extra_info" => col!(c, o => Value::List(
            c.world.comments_of(o.target)
                .map(|x| Value::List(vec![
                    Value::Int(x.id as i64),
                    Value::str(&x.author),
                    Value::str(&x.text),
                    Value::Int(x.entry_type as i64),
                    Value::Int(x.entry_time),
                ]))
                .collect()
        )),
        "downtimes" => col!(c, o => ids(c.world.downtimes_of(o.target).map(|x| x.id))),
        "downtimes_with_info" => col!(c, o => Value::List(
            c.world.downtimes_of(o.target)
                .map(|x| Value::List(vec![Value::Int(x.id as i64), Value::str(&x.author), Value::str(&x.comment)]))
                .collect()
        )),
        "downtimes_with_extra_info" => col!(c, o => Value::List(
            c.world.downtimes_of(o.target)
                .map(|x| Value::List(vec![
                    Value::Int(x.id as i64),
                    Value::str(&x.author),
                    Value::str(&x.comment),
                    Value::Int(0),
                    Value::Int(x.entry_time),
                    Value::Int(x.start_time),
                    Value::Int(x.end_time),
                    Value::bool(x.fixed),
                    Value::Int(x.duration),
                    Value::Int(0),
                    Value::bool(!x.is_in_effect),
                ]))
                .collect()
        )),
        // Nagios prepends to its list of contact groups: the last one configured comes first
        "contact_groups" => col!(c, o => Value::str_list(o.cfg.contact_groups.iter().rev().cloned())),
        "contacts" => col!(c, o => {
            let ids = match o.svc { Some(s) => &s.contacts, None => &o.host.contacts };
            Value::str_list(ids.iter().map(|id| c.world.contact(*id).cfg.name.clone()))
        }),
        "current_attempt" => col!(c, o => Value::Int(o.st.current_attempt as i64)),
        "current_notification_number" => col!(c, o => Value::Int(o.st.current_notification_number as i64)),
        "custom_variable_names" => {
            col!(c, o => Value::str_list(labels::custom_only(&o.cfg.custom_vars).map(|(k, _)| k.clone())))
        }
        "custom_variable_values" => {
            col!(c, o => Value::str_list(labels::custom_only(&o.cfg.custom_vars).map(|(_, v)| v.clone())))
        }
        "custom_variables" => {
            col!(c, o => Value::str_dict(labels::custom_only(&o.cfg.custom_vars).map(|(k, v)| (k.clone(), v.clone()))))
        }
        "labels" => col!(c, o => Value::str_dict(labels::split(&o.cfg.custom_vars).labels)),
        "label_names" => {
            col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).labels.into_iter().map(|x| x.0)))
        }
        "label_values" => {
            col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).labels.into_iter().map(|x| x.1)))
        }
        "label_sources" => col!(c, o => Value::str_dict(labels::split(&o.cfg.custom_vars).label_sources)),
        "label_source_names" => {
            col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).label_sources.into_iter().map(|x| x.0)))
        }
        "label_source_values" => {
            col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).label_sources.into_iter().map(|x| x.1)))
        }
        "tags" => col!(c, o => Value::str_dict(labels::split(&o.cfg.custom_vars).tags)),
        "tag_names" => col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).tags.into_iter().map(|x| x.0))),
        "tag_values" => col!(c, o => Value::str_list(labels::split(&o.cfg.custom_vars).tags.into_iter().map(|x| x.1))),
        "display_name" => col!(c, o => Value::str(&o.cfg.display_name)),
        "event_handler" => {
            col!(c, o => Value::Str(o.cfg.event_handler.as_ref().map(|x| x.raw.clone()).unwrap_or_default()))
        }
        "event_handler_enabled" => col!(c, o => Value::bool(o.st.event_handler_enabled)),
        "execution_time" => col!(c, o => Value::Float(o.st.execution_time)),
        "first_notification_delay" => col!(c, o => Value::Float(o.cfg.first_notification_delay)),
        "flap_detection_enabled" => col!(c, o => Value::bool(o.st.flap_detection_enabled)),
        "flappiness" | "percent_state_change" => col!(c, o => Value::Float(o.st.percent_state_change)),
        "hard_state" => col!(c, o => Value::Int(if o.st.current_state == 0 { 0 } else { o.st.hard_state() as i64 })),
        "has_been_checked" => col!(c, o => Value::bool(o.st.has_been_checked)),
        "high_flap_threshold" => col!(c, o => Value::Float(o.cfg.high_flap_threshold)),
        "low_flap_threshold" => col!(c, o => Value::Float(o.cfg.low_flap_threshold)),
        "icon_image" => col!(c, o => Value::str(&o.cfg.icon_image)),
        "icon_image_alt" => col!(c, o => Value::str(&o.cfg.icon_image_alt)),
        "icon_image_expanded" => {
            col!(c, o => Value::Str(corax_core::macros::expand_url(c.world, o.target, &o.cfg.icon_image)))
        }
        "in_check_period" => col!(c, o => Value::bool(c.timeperiod_active(o.cfg.check_period.as_deref()))),
        "in_notification_period" => {
            col!(c, o => Value::bool(c.timeperiod_active(o.cfg.notification_period.as_deref())))
        }
        "in_service_period" => col!(c, o => Value::bool(c.timeperiod_active(o.cfg.custom_var("SERVICE_PERIOD")))),
        "service_period" => col!(c, o => Value::str(o.cfg.custom_var("SERVICE_PERIOD").unwrap_or_default())),
        "is_executing" => col!(c, o => Value::bool(o.st.is_executing)),
        "is_flapping" => col!(c, o => Value::bool(o.st.is_flapping)),
        "last_check" => col!(c, o => Value::Int(o.st.last_check)),
        "last_hard_state" => col!(c, o => Value::Int(o.st.last_hard_state as i64)),
        "last_hard_state_change" => col!(c, o => Value::Int(o.st.last_hard_state_change)),
        "last_notification" => col!(c, o => Value::Int(o.st.last_notification)),
        "last_state" => col!(c, o => Value::Int(o.st.last_state as i64)),
        "last_state_change" => col!(c, o => Value::Int(o.st.last_state_change)),
        "latency" => col!(c, o => Value::Float(o.st.latency)),
        "long_plugin_output" => col!(c, o => Value::Str(escaped_long_output(&o.st.long_plugin_output))),
        "max_check_attempts" => col!(c, o => Value::Int(o.cfg.max_check_attempts as i64)),
        "metrics" => col!(c, o => Value::str_list(perf_values(&o.st.perf_data).into_iter().map(|x| x.0))),
        "modified_attributes" => col!(c, o => Value::Int(o.st.modified_attributes as i64)),
        "modified_attributes_list" => col!(c, o => modified_list(o.st.modified_attributes)),
        "next_check" => col!(c, o => Value::Int(o.st.next_check)),
        "next_notification" => col!(c, o => Value::Int(o.st.next_notification)),
        "no_more_notifications" => col!(c, o => Value::bool(o.st.no_more_notifications)),
        "notes" => col!(c, o => Value::str(&o.cfg.notes)),
        "notes_expanded" => col!(c, o => Value::Str(corax_core::macros::expand_url(c.world, o.target, &o.cfg.notes))),
        "notes_url" => col!(c, o => Value::str(&o.cfg.notes_url)),
        "notes_url_expanded" => {
            col!(c, o => Value::Str(corax_core::macros::expand_url(c.world, o.target, &o.cfg.notes_url)))
        }
        "notification_interval" => col!(c, o => Value::Float(o.cfg.notification_interval)),
        "notification_period" => col!(c, o => Value::Str(o.cfg.notification_period.clone().unwrap_or_default())),
        "notification_postponement_reason" => col!(c, o => Value::str(&o.st.notification_postponement_reason)),
        "notifications_enabled" => col!(c, o => Value::bool(o.st.notifications_enabled)),
        "pending_flex_downtime" => col!(c, o => Value::Int(pending_flex(c, o.target))),
        "perf_data" => col!(c, o => Value::str(&o.st.perf_data)),
        "performance_data" => col!(c, o => Value::Dict(
            perf_values(&o.st.perf_data).into_iter().map(|(k, v)| (k, Value::Float(v))).collect()
        )),
        "plugin_output" => col!(c, o => Value::str(&o.st.plugin_output)),
        "previous_hard_state" => col!(c, o => Value::Int(-1)),
        "process_performance_data" => col!(c, o => Value::bool(o.st.process_performance_data)),
        "scheduled_downtime_depth" => col!(c, o => Value::Int(o.st.scheduled_downtime_depth as i64)),
        "state" => col!(c, o => Value::Int(o.st.current_state as i64)),
        "state_type" => col!(c, o => Value::Int(o.st.state_type as i64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfdata_parsing() {
        let p = perf_values("rta=0.012ms;200.000;500.000;0; 'a b'=3;;;; pl=0%;80;100;0;100 junk util=1,5");
        assert_eq!(p, vec![("rta".into(), 0.012), ("a b".into(), 3.0), ("pl".into(), 0.0), ("util".into(), 1.5)]);
        assert!(perf_values("").is_empty());
    }

    #[test]
    fn cleanup() {
        assert_eq!(pnp_cleanup("Filesystem /var"), "Filesystem__var");
    }
}
