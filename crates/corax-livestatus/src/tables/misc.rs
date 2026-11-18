//! Small tables: columns, commands, timeperiods, contactgroups, crashreports, labels.

use std::collections::BTreeSet;
use std::path::Path;

use corax_core::World;

use super::{getter, Getter, Row};
use crate::labels;
use crate::value::Value;

/// A crash report directory `<crash_reports_path>/<component>/<id>`.
#[derive(Debug, Clone)]
pub struct CrashReport {
    pub component: String,
    pub id: String,
}

/// One distinct label of any host or service.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LabelRow {
    pub name: String,
    pub value: String,
}

macro_rules! own {
    ($variant:ident, $x:ident => $e:expr) => {
        Some(getter(move |_c, r| match r {
            Row::$variant($x) => $e,
            _ => Value::Null,
        }))
    };
}

pub fn columns_column(name: &str) -> Option<Getter> {
    match name {
        "table" => own!(Column, x => Value::str(x.table)),
        "name" => own!(Column, x => Value::str(x.name)),
        "type" => own!(Column, x => Value::str(x.ty.name())),
        "description" => own!(Column, x => Value::str(x.description)),
        _ => None,
    }
}

pub fn command_column(name: &str) -> Option<Getter> {
    match name {
        "name" => own!(Command, x => Value::str(&x.name)),
        "line" => own!(Command, x => Value::str(&x.line)),
        _ => None,
    }
}

pub fn timeperiod_column(name: &str) -> Option<Getter> {
    match name {
        "name" => own!(Timeperiod, x => Value::str(&x.name)),
        "alias" => own!(Timeperiod, x => Value::str(&x.alias)),
        "in" => Some(getter(|c, r| match r {
            Row::Timeperiod(x) => Value::bool(c.timeperiod_active(Some(&x.name))),
            _ => Value::Null,
        })),
        "next_transition" => own!(Timeperiod, _x => Value::Int(0)),
        "next_transition_id" => own!(Timeperiod, _x => Value::Int(1)),
        "num_transitions" => own!(Timeperiod, _x => Value::Int(2)),
        "transitions" => own!(Timeperiod, _x => Value::List(Vec::new())),
        _ => None,
    }
}

pub fn contactgroup_column(name: &str) -> Option<Getter> {
    match name {
        "name" => own!(ContactGroup, x => Value::str(&x.cfg.name)),
        "alias" => own!(ContactGroup, x => Value::str(&x.cfg.alias)),
        "members" => Some(getter(|c, r| match r {
            Row::ContactGroup(x) => Value::str_list(x.members.iter().map(|m| c.world.contact(*m).cfg.name.clone())),
            _ => Value::Null,
        })),
        _ => None,
    }
}

pub fn crashreport_column(name: &str) -> Option<Getter> {
    match name {
        "component" => own!(CrashReport, x => Value::str(&x.component)),
        "id" => own!(CrashReport, x => Value::str(&x.id)),
        _ => None,
    }
}

pub fn label_column(name: &str) -> Option<Getter> {
    match name {
        "name" => own!(Label, x => Value::str(&x.name)),
        "value" => own!(Label, x => Value::str(&x.value)),
        _ => None,
    }
}

pub fn load_crash_reports(dir: Option<&Path>) -> Vec<CrashReport> {
    let Some(dir) = dir else { return Vec::new() };
    let mut out = Vec::new();
    let Ok(components) = std::fs::read_dir(dir) else { return out };
    for comp in components.filter_map(Result::ok).filter(|e| e.path().is_dir()) {
        let component = comp.file_name().to_string_lossy().into_owned();
        let Ok(ids) = std::fs::read_dir(comp.path()) else { continue };
        for id in ids.filter_map(Result::ok).filter(|e| e.path().is_dir()) {
            out.push(CrashReport { component: component.clone(), id: id.file_name().to_string_lossy().into_owned() });
        }
    }
    out.sort_by(|a, b| (&a.component, &a.id).cmp(&(&b.component, &b.id)));
    out
}

pub fn collect_labels(world: &World) -> Vec<LabelRow> {
    let mut set = BTreeSet::new();
    let all = world.hosts.iter().map(|h| &h.cfg.common).chain(world.services.iter().map(|s| &s.cfg.common));
    for cfg in all {
        for (name, value) in labels::split(&cfg.custom_vars).labels {
            set.insert(LabelRow { name, value });
        }
    }
    set.into_iter().collect()
}
