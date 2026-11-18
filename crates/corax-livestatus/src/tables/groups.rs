//! Columns of the `hostgroups` and `servicegroups` tables.

use std::sync::Arc;

use corax_core::world::{HostGroup, ServiceGroup};

use super::hosts::SvcAgg;
use super::Ctx;
use crate::value::Value;

pub type HostGroupFn = Arc<dyn for<'x> Fn(&Ctx<'x>, &HostGroup) -> Value + Send + Sync>;
pub type ServiceGroupFn = Arc<dyn for<'x> Fn(&Ctx<'x>, &ServiceGroup) -> Value + Send + Sync>;

fn hg<F>(f: F) -> Option<HostGroupFn>
where
    F: for<'x> Fn(&Ctx<'x>, &HostGroup) -> Value + Send + Sync + 'static,
{
    Some(Arc::new(f))
}

fn sg<F>(f: F) -> Option<ServiceGroupFn>
where
    F: for<'x> Fn(&Ctx<'x>, &ServiceGroup) -> Value + Send + Sync + 'static,
{
    Some(Arc::new(f))
}

/// Severity order for "worst" host state: UP < UNREACHABLE < DOWN.
fn host_severity(state: u8) -> u8 {
    match state {
        0 => 0,
        2 => 1,
        _ => 2,
    }
}

#[derive(Clone, Copy)]
enum HostAgg {
    Num,
    Up,
    Down,
    Unreach,
    Pending,
    Handled,
    Unhandled,
    Worst,
}

fn host_agg(c: &Ctx<'_>, g: &HostGroup, agg: HostAgg) -> i64 {
    let mut result = 0i64;
    for h in g.members.iter().map(|id| c.world.host(*id)).filter(|h| c.host_visible(h)) {
        let st = &h.st;
        let handled = st.acknowledged || st.scheduled_downtime_depth > 0;
        let checked = st.has_been_checked;
        let hit = match agg {
            HostAgg::Num => true,
            HostAgg::Up => checked && st.current_state == 0,
            HostAgg::Down => checked && st.current_state == 1,
            HostAgg::Unreach => checked && st.current_state == 2,
            HostAgg::Pending => !checked,
            HostAgg::Handled => checked && st.current_state != 0 && handled,
            HostAgg::Unhandled => checked && st.current_state != 0 && !handled,
            HostAgg::Worst => {
                if host_severity(st.current_state) > host_severity(result as u8) {
                    result = st.current_state as i64;
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

pub fn hostgroup_column(name: &str) -> Option<HostGroupFn> {
    let host_agg_col = |a: HostAgg| hg(move |c, g| Value::Int(host_agg(c, g, a)));
    match name {
        "name" => hg(|_, g| Value::str(&g.cfg.name)),
        "alias" => hg(|_, g| Value::str(&g.cfg.alias)),
        "notes" => hg(|_, g| Value::str(&g.cfg.notes)),
        "notes_url" => hg(|_, g| Value::str(&g.cfg.notes_url)),
        "action_url" => hg(|_, g| Value::str(&g.cfg.action_url)),
        "members" => hg(|c, g| {
            Value::str_list(
                g.members.iter().map(|id| c.world.host(*id)).filter(|h| c.host_visible(h)).map(|h| h.cfg.name.clone()),
            )
        }),
        "members_with_state" => hg(|c, g| {
            Value::List(
                g.members
                    .iter()
                    .map(|id| c.world.host(*id))
                    .filter(|h| c.host_visible(h))
                    .map(|h| {
                        Value::List(vec![
                            Value::str(&h.cfg.name),
                            Value::Int(h.st.current_state as i64),
                            Value::bool(h.st.has_been_checked),
                        ])
                    })
                    .collect(),
            )
        }),
        "num_hosts" => host_agg_col(HostAgg::Num),
        "num_hosts_up" => host_agg_col(HostAgg::Up),
        "num_hosts_down" => host_agg_col(HostAgg::Down),
        "num_hosts_unreach" => host_agg_col(HostAgg::Unreach),
        "num_hosts_pending" => host_agg_col(HostAgg::Pending),
        "num_hosts_handled_problems" => host_agg_col(HostAgg::Handled),
        "num_hosts_unhandled_problems" => host_agg_col(HostAgg::Unhandled),
        "worst_host_state" => host_agg_col(HostAgg::Worst),
        _ => {
            let agg = SvcAgg::parse(name)?;
            hg(move |c, g| {
                let services = g
                    .members
                    .iter()
                    .map(|id| c.world.host(*id))
                    .filter(|h| c.host_visible(h))
                    .flat_map(|h| h.services.iter().map(|s| c.world.service(*s)));
                Value::Int(agg.compute(c, services))
            })
        }
    }
}

pub fn servicegroup_column(name: &str) -> Option<ServiceGroupFn> {
    match name {
        "name" => sg(|_, g| Value::str(&g.cfg.name)),
        "alias" => sg(|_, g| Value::str(&g.cfg.alias)),
        "notes" => sg(|_, g| Value::str(&g.cfg.notes)),
        "notes_url" => sg(|_, g| Value::str(&g.cfg.notes_url)),
        "action_url" => sg(|_, g| Value::str(&g.cfg.action_url)),
        "members" => sg(|c, g| {
            Value::List(
                g.members
                    .iter()
                    .map(|id| c.world.service(*id))
                    .filter(|s| c.service_visible(s))
                    .map(|s| {
                        Value::List(vec![Value::str(&c.world.host(s.host).cfg.name), Value::str(&s.cfg.description)])
                    })
                    .collect(),
            )
        }),
        "members_with_state" => sg(|c, g| {
            Value::List(
                g.members
                    .iter()
                    .map(|id| c.world.service(*id))
                    .filter(|s| c.service_visible(s))
                    .map(|s| {
                        Value::List(vec![
                            Value::str(&c.world.host(s.host).cfg.name),
                            Value::str(&s.cfg.description),
                            Value::Int(s.st.current_state as i64),
                            Value::bool(s.st.has_been_checked),
                        ])
                    })
                    .collect(),
            )
        }),
        _ => {
            let agg = SvcAgg::parse(name)?;
            sg(move |c, g| Value::Int(agg.compute(c, g.members.iter().map(|id| c.world.service(*id)))))
        }
    }
}
