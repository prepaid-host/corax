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
        "livestatus_threads" => s(|_| Value::Int(crate::server::MAX_CONNECTIONS as i64)),
        "livestatus_usage" => Some(getter(move |_, _| {
            Value::Float(stats.active_connections() as f64 / crate::server::MAX_CONNECTIONS as f64)
        })),
        "requests_rate" => Some(getter(move |_, _| Value::Float(stats.rates().0))),
        "connections_rate" => Some(getter(move |_, _| Value::Float(stats.rates().1))),
        "service_checks_rate" => s(|c| Value::Float(c.world.counters.service_checks_rate.rate())),
        "host_checks_rate" => s(|c| Value::Float(c.world.counters.host_checks_rate.rate())),
        "external_commands_rate" => s(|c| Value::Float(c.world.counters.external_commands_rate.rate())),
        "log_messages_rate" => s(|c| Value::Float(c.world.counters.log_messages_rate.rate())),
        "forks_rate" => s(|c| Value::Float(c.world.counters.forks_rate.rate())),
        "average_latency_generic" | "average_latency_service" => s(|c| {
            let (sum, n) = c
                .world
                .services
                .iter()
                .filter(|s| s.st.has_been_checked && s.st.check_type == corax_core::CheckType::Active)
                .fold((0.0, 0u32), |(sum, n), s| (sum + s.st.latency, n + 1));
            Value::Float(if n > 0 { sum / n as f64 } else { 0.0 })
        }),
        "license_usage_history" => s(|c| {
            let text = c.paths.license_usage_history_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
            Value::str(text.as_deref().unwrap_or("").trim_end())
        }),
        "state_file_created" => s(|c| {
            let ts = c.paths.state_file_created_file.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
            Value::Int(ts.and_then(|t| t.trim().parse::<f64>().ok()).unwrap_or(0.0) as i64)
        }),
        "external_command_buffer_slots" | "external_command_buffer_max" => s(|_| Value::Int(32768)),
        _ => None,
    }
}
