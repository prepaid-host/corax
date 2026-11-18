//! Columns of the `hosts` table (also joined as `host_*` elsewhere).

use std::sync::Arc;

use corax_core::{Host, Service};

use super::checkable::{self, pnp_cleanup, Obj};
use super::Ctx;
use crate::value::Value;

pub type HostFn = Arc<dyn for<'x> Fn(&Ctx<'x>, &Host) -> Value + Send + Sync>;

fn f<F>(f: F) -> Option<HostFn>
where
    F: for<'x> Fn(&Ctx<'x>, &Host) -> Value + Send + Sync + 'static,
{
    Some(Arc::new(f))
}

/// Severity order for "worst" service state: OK < WARN < UNKNOWN < CRIT.
pub fn service_severity(state: u8) -> u8 {
    match state {
        0 => 0,
        1 => 1,
        3 => 2,
        _ => 3,
    }
}

/// Aggregations over a set of services (Livestatus `ServiceListState`).
#[derive(Debug, Clone, Copy)]
pub enum SvcAgg {
    Num,
    Pending,
    HandledProblems,
    UnhandledProblems,
    Ok,
    Warn,
    Crit,
    Unknown,
    WorstState,
    HardOk,
    HardWarn,
    HardCrit,
    HardUnknown,
    WorstHardState,
}

impl SvcAgg {
    pub fn parse(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "num_services" | "total_services" => Self::Num,
            "num_services_pending" => Self::Pending,
            "num_services_handled_problems" => Self::HandledProblems,
            "num_services_unhandled_problems" => Self::UnhandledProblems,
            "num_services_ok" => Self::Ok,
            "num_services_warn" => Self::Warn,
            "num_services_crit" => Self::Crit,
            "num_services_unknown" => Self::Unknown,
            "worst_service_state" => Self::WorstState,
            "num_services_hard_ok" => Self::HardOk,
            "num_services_hard_warn" => Self::HardWarn,
            "num_services_hard_crit" => Self::HardCrit,
            "num_services_hard_unknown" => Self::HardUnknown,
            "worst_service_hard_state" => Self::WorstHardState,
            _ => return None,
        })
    }

    pub fn compute<'a>(self, ctx: &Ctx<'_>, services: impl Iterator<Item = &'a Service>) -> i64 {
        let mut result: i64 = 0;
        for s in services.filter(|s| ctx.service_visible(s)) {
            let st = &s.st;
            let cur = st.current_state;
            let hard = st.last_hard_state;
            let checked = st.has_been_checked;
            let handled = st.acknowledged || st.scheduled_downtime_depth > 0;
            let hit = match self {
                Self::Num => true,
                Self::Pending => !checked,
                Self::HandledProblems => checked && cur != 0 && handled,
                Self::UnhandledProblems => checked && cur != 0 && !handled,
                Self::Ok => checked && cur == 0,
                Self::Warn => checked && cur == 1,
                Self::Crit => checked && cur == 2,
                Self::Unknown => checked && cur == 3,
                Self::HardOk => checked && hard == 0,
                Self::HardWarn => checked && hard == 1,
                Self::HardCrit => checked && hard == 2,
                Self::HardUnknown => checked && hard == 3,
                Self::WorstState | Self::WorstHardState => {
                    let state = if matches!(self, Self::WorstState) { cur } else { hard };
                    if service_severity(state) > service_severity(result as u8) {
                        result = state as i64;
                    }
                    false
                }
            };
            if hit {
                result += 1;
            }
        }
        result
    }
}

fn services_of<'a>(ctx: &Ctx<'a>, h: &'a Host) -> impl Iterator<Item = &'a Service> + 'a {
    let w = ctx.world;
    h.services.iter().map(move |s| w.service(*s))
}

fn visible_services<'a>(ctx: &'a Ctx<'a>, h: &'a Host) -> impl Iterator<Item = &'a Service> + 'a {
    services_of(ctx, h).filter(move |s| ctx.service_visible(s))
}

fn host_staleness(c: &Ctx<'_>, h: &Host) -> f64 {
    let age = (c.now - h.st.last_check) as f64;
    let interval = if h.cfg.common.check_interval != 0.0 { h.cfg.common.check_interval } else { 1.0 };
    age / (interval * c.interval_length() as f64)
}

pub fn column(name: &str) -> Option<HostFn> {
    if let Some(agg) = SvcAgg::parse(name) {
        return f(move |c, h| Value::Int(agg.compute(c, services_of(c, h))));
    }
    match name {
        "name" => f(|_, h| Value::str(&h.cfg.name)),
        "alias" => f(|_, h| Value::str(&h.cfg.alias)),
        "address" => f(|_, h| Value::str(&h.cfg.address)),
        "filename" => f(|_, h| Value::str(h.cfg.common.custom_var("FILENAME").unwrap_or_default())),
        "groups" => f(|_, h| Value::str_list(h.cfg.hostgroups.iter().cloned())),
        "parents" => f(|_, h| Value::str_list(h.cfg.parents.iter().cloned())),
        "childs" => f(|c, h| Value::str_list(h.children.iter().map(|x| c.world.host(*x).cfg.name.clone()))),
        "statusmap_image" => f(|_, h| Value::str(&h.cfg.statusmap_image)),
        "obsess_over_host" => f(|_, h| Value::bool(h.st.obsess)),
        "last_time_up" => f(|_, h| Value::Int(h.st.last_time_in_state[0])),
        "last_time_down" => f(|_, h| Value::Int(h.st.last_time_in_state[1])),
        "last_time_unreachable" => f(|_, h| Value::Int(h.st.last_time_in_state[2])),
        "smartping_timeout" => f(|_, h| Value::Int((h.cfg.common.check_interval * 60000.0 * 2.5) as i64)),
        "staleness" => f(|c, h| Value::Float(host_staleness(c, h))),
        "x_3d" | "y_3d" | "z_3d" => f(|_, _| Value::Float(0.0)),
        "structured_status" => f(|_, _| Value::Blob(Vec::new())),
        "pnpgraph_present" => f(|c, h| Value::bool(c.pnp_graph_present(&pnp_cleanup(&h.cfg.name), "_HOST_.xml"))),
        "mk_inventory" => f(|c, h| inventory_file(c, h, "")),
        "mk_inventory_gz" => f(|c, h| inventory_file(c, h, ".gz")),
        "mk_inventory_last" => f(|c, h| {
            let mtime = c
                .paths
                .mk_inventory_path
                .as_ref()
                .and_then(|p| std::fs::metadata(p.join(&h.cfg.name)).ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Value::Int(mtime)
        }),
        "mk_logwatch_files" => f(|c, h| {
            let mut files: Vec<String> = c
                .paths
                .mk_logwatch_path
                .as_ref()
                .and_then(|p| std::fs::read_dir(p.join(&h.cfg.name)).ok())
                .map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            files.sort();
            Value::str_list(files)
        }),
        "services" => f(|c, h| Value::str_list(visible_services(c, h).map(|s| s.cfg.description.clone()))),
        "services_with_state" => f(|c, h| {
            Value::List(
                visible_services(c, h)
                    .map(|s| {
                        Value::List(vec![
                            Value::str(&s.cfg.description),
                            Value::Int(s.st.current_state as i64),
                            Value::bool(s.st.has_been_checked),
                        ])
                    })
                    .collect(),
            )
        }),
        "services_with_info" => f(|c, h| {
            Value::List(
                visible_services(c, h)
                    .map(|s| {
                        Value::List(vec![
                            Value::str(&s.cfg.description),
                            Value::Int(s.st.current_state as i64),
                            Value::bool(s.st.has_been_checked),
                            Value::str(&s.st.plugin_output),
                        ])
                    })
                    .collect(),
            )
        }),
        "services_with_fullstate" => f(|c, h| {
            Value::List(
                visible_services(c, h)
                    .map(|s| {
                        let st = &s.st;
                        Value::List(vec![
                            Value::str(&s.cfg.description),
                            Value::Int(st.current_state as i64),
                            Value::bool(st.has_been_checked),
                            Value::str(&st.plugin_output),
                            Value::Int(if st.current_state == 0 { 0 } else { st.hard_state() as i64 }),
                            Value::Int(st.current_attempt as i64),
                            Value::Int(s.cfg.common.max_check_attempts as i64),
                            Value::Int(st.scheduled_downtime_depth as i64),
                            Value::bool(st.acknowledged),
                            Value::bool(c.timeperiod_active(s.cfg.common.custom_var("SERVICE_PERIOD"))),
                        ])
                    })
                    .collect(),
            )
        }),
        _ => {
            let g = checkable::column(name)?;
            f(move |c, h| g(c, &Obj::host(h)))
        }
    }
}

fn inventory_file(c: &Ctx<'_>, h: &Host, suffix: &str) -> Value {
    let data = c
        .paths
        .mk_inventory_path
        .as_ref()
        .and_then(|p| std::fs::read(p.join(format!("{}{suffix}", h.cfg.name))).ok())
        .unwrap_or_default();
    Value::Blob(data)
}
