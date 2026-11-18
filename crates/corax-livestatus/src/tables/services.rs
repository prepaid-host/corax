//! Columns of the `services` table (also joined as `service_*` elsewhere).

use std::sync::Arc;

use corax_core::Service;

use super::checkable::{self, pnp_cleanup, Obj};
use super::Ctx;
use crate::value::Value;

pub type ServiceFn = Arc<dyn for<'x> Fn(&Ctx<'x>, &Service) -> Value + Send + Sync>;

fn f<F>(f: F) -> Option<ServiceFn>
where
    F: for<'x> Fn(&Ctx<'x>, &Service) -> Value + Send + Sync + 'static,
{
    Some(Arc::new(f))
}

fn is_cmk_passive(s: &Service) -> bool {
    s.cfg.common.check_command.as_ref().is_some_and(|c| c.name.starts_with("check_mk-"))
}

/// Staleness as computed by Livestatus for the Nagios core.
fn staleness(c: &Ctx<'_>, s: &Service) -> f64 {
    let age = (c.now - s.st.last_check) as f64;
    let il = c.interval_length() as f64;
    if s.cfg.common.check_interval != 0.0 {
        return age / (s.cfg.common.check_interval * il);
    }
    if is_cmk_passive(s) {
        let host = c.world.host(s.host);
        for other in host.services.iter().map(|id| c.world.service(*id)) {
            if is_cmk_passive(other) {
                let iv = if other.cfg.common.check_interval == 0.0 { 1.0 } else { other.cfg.common.check_interval };
                return age / (iv * il);
            }
        }
        return 1.0;
    }
    age / il
}

pub fn column(name: &str) -> Option<ServiceFn> {
    match name {
        "description" => f(|_, s| Value::str(&s.cfg.description)),
        "groups" => f(|_, s| Value::str_list(s.cfg.servicegroups.iter().cloned())),
        "obsess_over_service" => f(|_, s| Value::bool(s.st.obsess)),
        "last_time_ok" => f(|_, s| Value::Int(s.st.last_time_in_state[0])),
        "last_time_warning" => f(|_, s| Value::Int(s.st.last_time_in_state[1])),
        "last_time_critical" => f(|_, s| Value::Int(s.st.last_time_in_state[2])),
        "last_time_unknown" => f(|_, s| Value::Int(s.st.last_time_in_state[3])),
        "cached_at" => f(|_, s| Value::Int(s.st.cached_at)),
        "cache_interval" => f(|_, s| Value::Int(s.st.cache_interval)),
        "passive_check_period" => f(|_, s| Value::Str(s.cfg.common.check_period.clone().unwrap_or_default())),
        "in_passive_check_period" => f(|c, s| Value::bool(c.timeperiod_active(s.cfg.common.check_period.as_deref()))),
        "staleness" => f(|c, s| Value::Float(staleness(c, s))),
        "pnpgraph_present" => f(|c, s| {
            let host = &c.world.host(s.host).cfg.name;
            Value::bool(c.pnp_graph_present(&pnp_cleanup(host), &format!("{}.xml", pnp_cleanup(&s.cfg.description))))
        }),
        "prediction_files" => f(|_, _| Value::List(Vec::new())),
        "robotmk_last_log" | "robotmk_last_log_gz" | "robotmk_last_error_log" | "robotmk_last_error_log_gz" => {
            f(|_, _| Value::Blob(Vec::new()))
        }
        _ => {
            let g = checkable::column(name)?;
            f(move |c, s| g(c, &Obj::service(c, s)))
        }
    }
}
