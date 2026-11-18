//! The single-row `status` table.

use super::{getter, Ctx, Getter};
use crate::value::Value;

fn s<F>(f: F) -> Option<Getter>
where
    F: for<'a> Fn(&Ctx<'a>) -> Value + Send + Sync + 'static,
{
    Some(getter(move |c, _| f(c)))
}

/// Fallback `livestatus_version` outside of an OMD site. Inside a site the Checkmk
/// version is reported, because the GUI parses this column as a Checkmk version.
pub const LIVESTATUS_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn column(name: &str) -> Option<Getter> {
    let stats = crate::stats();
    match name {
        "program_version" => s(|c| Value::str(&c.world.version)),
        "livestatus_version" => s(|c| Value::str(c.paths.checkmk_version.as_deref().unwrap_or(LIVESTATUS_VERSION))),
        "program_start" => s(|c| Value::Int(c.world.program.program_start)),
        "nagios_pid" | "core_pid" => s(|c| Value::Int(c.world.program.pid as i64)),
        "edition" => s(|c| Value::str(&c.paths.edition)),
        "num_hosts" => s(|c| Value::Int(c.world.hosts.len() as i64)),
        "num_services" => s(|c| Value::Int(c.world.services.len() as i64)),
        "interval_length" => s(|c| Value::Int(c.world.settings.interval_length)),
        "enable_notifications" => s(|c| Value::bool(c.world.program.enable_notifications)),
        "execute_service_checks" => s(|c| Value::bool(c.world.program.execute_service_checks)),
        "accept_passive_service_checks" => s(|c| Value::bool(c.world.program.accept_passive_service_checks)),
        "execute_host_checks" => s(|c| Value::bool(c.world.program.execute_host_checks)),
        "accept_passive_host_checks" => s(|c| Value::bool(c.world.program.accept_passive_host_checks)),
        "enable_event_handlers" => s(|c| Value::bool(c.world.program.enable_event_handlers)),
        "enable_flap_detection" => s(|c| Value::bool(c.world.program.enable_flap_detection)),
        "process_performance_data" => s(|c| Value::bool(c.world.program.process_performance_data)),
        "check_service_freshness" => s(|c| Value::bool(c.world.program.check_service_freshness)),
        "check_host_freshness" => s(|c| Value::bool(c.world.program.check_host_freshness)),
        "obsess_over_services" => s(|c| Value::bool(c.world.program.obsess_over_services)),
        "obsess_over_hosts" => s(|c| Value::bool(c.world.program.obsess_over_hosts)),
        "check_external_commands" => s(|_| Value::Int(1)),
        "last_command_check" => s(|c| Value::Int(c.world.program.last_command_check)),
        "last_log_rotation" => s(|c| Value::Int(c.world.program.last_log_rotation)),
        "has_event_handlers" => s(|c| {
            Value::bool(
                c.world.hosts.iter().any(|h| h.cfg.common.event_handler.is_some())
                    || c.world.services.iter().any(|x| x.cfg.common.event_handler.is_some()),
            )
        }),
        "service_checks" => s(|c| Value::Float(c.world.counters.service_checks as f64)),
        "host_checks" => s(|c| Value::Float(c.world.counters.host_checks as f64)),
        "external_commands" => s(|c| Value::Float(c.world.counters.external_commands as f64)),
        "log_messages" => s(|c| Value::Float(c.world.counters.log_messages as f64)),
        "forks" => s(|c| Value::Float(c.world.counters.forks as f64)),
        "neb_callbacks" => s(|c| Value::Float(c.world.counters.neb_callbacks as f64)),
        "requests" => Some(getter(move |_, _| Value::Float(stats.queries() as f64))),
        "connections" => Some(getter(move |_, _| Value::Float(stats.connections() as f64))),
        "livestatus_active_connections" => Some(getter(move |_, _| Value::Int(stats.active_connections() as i64))),
        "livestatus_threads" => s(|_| Value::Int(0)),
        "external_command_buffer_slots" | "external_command_buffer_max" => s(|_| Value::Int(32768)),
        _ => None,
    }
}
