//! Tables, rows and column getters.
//!
//! Every table is described by the embedded [`crate::schema`]. A column getter is
//! built per requested column; it knows how to extract its value from any [`Row`] of
//! the table, including joined objects (e.g. `host_*` columns in the `services`
//! table). Columns that corax cannot fill return the type's default value.

mod checkable;
mod comments;
mod contacts;
mod groups;
mod hosts;
pub mod log;
mod misc;
mod services;
mod status;

use std::path::Path;
use std::sync::Arc;

use corax_core::world::{ContactGroup, HostGroup, ServiceGroup};
use corax_core::{Comment, Contact, ContactId, Downtime, Host, Paths, Service, Target, World};

use crate::schema::{self, ColumnDef};
use crate::value::{ColumnType, Value};

pub use log::LogEntry;
pub use misc::{CrashReport, LabelRow};

/// Who is asking (Livestatus `AuthUser`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// No `AuthUser` header: everything is visible.
    All,
    Contact(ContactId),
    /// `AuthUser` names a contact that does not exist: nothing is visible.
    Nobody,
}

/// Per-query evaluation context.
pub struct Ctx<'a> {
    pub world: &'a World,
    pub paths: &'a Paths,
    pub now: i64,
    pub auth: Auth,
}

impl<'a> Ctx<'a> {
    pub fn host_visible(&self, h: &Host) -> bool {
        match self.auth {
            Auth::All => true,
            Auth::Contact(c) => h.contacts.contains(&c),
            Auth::Nobody => false,
        }
    }

    /// Loose service authorization: contact of the service or of its host.
    pub fn service_visible(&self, s: &Service) -> bool {
        match self.auth {
            Auth::All => true,
            Auth::Contact(c) => s.contacts.contains(&c) || self.world.host(s.host).contacts.contains(&c),
            Auth::Nobody => false,
        }
    }

    pub fn target_visible(&self, t: Target) -> bool {
        match t {
            Target::Host(h) => self.host_visible(self.world.host(h)),
            Target::Service(s) => self.service_visible(self.world.service(s)),
        }
    }

    pub fn interval_length(&self) -> i64 {
        self.world.settings.interval_length
    }

    pub fn timeperiod_active(&self, name: Option<&str>) -> bool {
        corax_config::timeperiod::is_active(&self.world.objects.timeperiods, name, self.now)
    }
}

/// One row of some table.
#[derive(Clone, Copy)]
pub enum Row<'a> {
    Host(&'a Host),
    Service(&'a Service),
    HostGroup(&'a HostGroup),
    ServiceGroup(&'a ServiceGroup),
    Contact(&'a Contact),
    ContactGroup(&'a ContactGroup),
    Command(&'a corax_config::CommandConfig),
    Timeperiod(&'a corax_config::Timeperiod),
    Status,
    Column(&'a ColumnDef),
    Comment(&'a Comment),
    Downtime(&'a Downtime),
    Log(&'a LogEntry),
    HostByGroup(&'a Host, &'a HostGroup),
    ServiceByGroup(&'a Service, &'a ServiceGroup),
    ServiceByHostGroup(&'a Service, &'a HostGroup),
    CrashReport(&'a CrashReport),
    Label(&'a LabelRow),
}

impl<'a> Row<'a> {
    /// The host this row refers to (directly or joined).
    pub fn host(&self, ctx: &Ctx<'a>) -> Option<&'a Host> {
        let w = ctx.world;
        match *self {
            Row::Host(h) | Row::HostByGroup(h, _) => Some(h),
            Row::Service(s) | Row::ServiceByGroup(s, _) | Row::ServiceByHostGroup(s, _) => Some(w.host(s.host)),
            Row::Comment(c) => Some(w.host(w.host_of(c.target))),
            Row::Downtime(d) => Some(w.host(w.host_of(d.target))),
            Row::Log(l) => w.find_host(&l.host_name).map(|h| w.host(h)),
            _ => None,
        }
    }

    /// The service this row refers to (directly or joined).
    pub fn service(&self, ctx: &Ctx<'a>) -> Option<&'a Service> {
        let w = ctx.world;
        match *self {
            Row::Service(s) | Row::ServiceByGroup(s, _) | Row::ServiceByHostGroup(s, _) => Some(s),
            Row::Comment(Comment { target: Target::Service(s), .. }) => Some(w.service(*s)),
            Row::Downtime(Downtime { target: Target::Service(s), .. }) => Some(w.service(*s)),
            Row::Log(l) if !l.service_description.is_empty() => {
                w.find_service(&l.host_name, &l.service_description).map(|s| w.service(s))
            }
            _ => None,
        }
    }

    pub fn hostgroup(&self) -> Option<&'a HostGroup> {
        match *self {
            Row::HostGroup(g) | Row::HostByGroup(_, g) | Row::ServiceByHostGroup(_, g) => Some(g),
            _ => None,
        }
    }

    pub fn servicegroup(&self) -> Option<&'a ServiceGroup> {
        match *self {
            Row::ServiceGroup(g) | Row::ServiceByGroup(_, g) => Some(g),
            _ => None,
        }
    }

    pub fn contact(&self, ctx: &Ctx<'a>) -> Option<&'a Contact> {
        match *self {
            Row::Contact(c) => Some(c),
            Row::Log(l) if !l.contact_name.is_empty() => {
                ctx.world.contact_by_name.get(&l.contact_name).map(|c| ctx.world.contact(*c))
            }
            _ => None,
        }
    }
}

/// Extracts a value from a row.
pub type Getter = Arc<dyn for<'a> Fn(&Ctx<'a>, &Row<'a>) -> Value + Send + Sync>;

/// A resolved column of a query.
#[derive(Clone)]
pub struct Column {
    /// Name as requested by the client.
    pub name: String,
    /// Canonical schema name (`name` for a requested `host_name` in `hosts`).
    pub canonical: &'static str,
    pub ty: ColumnType,
    pub get: Getter,
    /// Set for dynamic `rrddata` columns: the getter yields the object key, which
    /// [`crate::rrd::resolve`] replaces with the data once the world lock is released.
    pub rrd: Option<Arc<crate::rrd::RrdArgs>>,
}

/// Something a table row can be joined with.
#[derive(Debug, Clone, Copy)]
enum Entity {
    Host,
    Service,
    HostGroup,
    ServiceGroup,
    Contact,
    /// Columns specific to the table itself.
    Own,
}

/// `(prefix, entity)` pairs, most specific first.
fn joins(table: &str) -> &'static [(&'static str, Entity)] {
    use Entity::*;
    match table {
        "hosts" => &[("", Host)],
        "services" => &[("host_", Host), ("", Service)],
        "hostgroups" => &[("", HostGroup)],
        "servicegroups" => &[("", ServiceGroup)],
        "contacts" => &[("", Contact)],
        "hostsbygroup" => &[("hostgroup_", HostGroup), ("", Host)],
        "servicesbygroup" => &[("servicegroup_", ServiceGroup), ("host_", Host), ("", Service)],
        "servicesbyhostgroup" => &[("hostgroup_", HostGroup), ("host_", Host), ("", Service)],
        "comments" | "downtimes" => &[("host_", Host), ("service_", Service), ("", Own)],
        "log" | "statehist" => {
            &[("current_host_", Host), ("current_service_", Service), ("current_contact_", Contact), ("", Own)]
        }
        "eventconsoleevents" | "eventconsolehistory" => &[("host_", Host), ("", Own)],
        _ => &[("", Own)],
    }
}

/// Wrap a closure into a [`Getter`] (helps closure signature inference).
pub fn getter<F>(f: F) -> Getter
where
    F: for<'a> Fn(&Ctx<'a>, &Row<'a>) -> Value + Send + Sync + 'static,
{
    Arc::new(f)
}

fn default_getter(ty: ColumnType) -> Getter {
    getter(move |_, _| ty.default_value())
}

fn null_getter() -> Getter {
    getter(|_, _| Value::Null)
}

/// Build the getter for a schema column of `table`.
fn build_getter(table: &str, def: &ColumnDef) -> Getter {
    let ty = def.ty;
    for &(prefix, entity) in joins(table) {
        let Some(rest) = def.name.strip_prefix(prefix) else { continue };
        let getter = match entity {
            Entity::Host => hosts::column(rest)
                .map(|f| getter(move |c, r| r.host(c).map(|h| f(c, h)).unwrap_or_else(|| ty.default_value()))),
            Entity::Service => services::column(rest)
                .map(|f| getter(move |c, r| r.service(c).map(|s| f(c, s)).unwrap_or_else(|| ty.default_value()))),
            Entity::HostGroup => groups::hostgroup_column(rest)
                .map(|f| getter(move |c, r| r.hostgroup().map(|g| f(c, g)).unwrap_or_else(|| ty.default_value()))),
            Entity::ServiceGroup => groups::servicegroup_column(rest)
                .map(|f| getter(move |c, r| r.servicegroup().map(|g| f(c, g)).unwrap_or_else(|| ty.default_value()))),
            Entity::Contact => contacts::column(rest)
                .map(|f| getter(move |c, r| r.contact(c).map(|ct| f(c, ct)).unwrap_or_else(|| ty.default_value()))),
            Entity::Own => own_column(table, rest),
        };
        if let Some(g) = getter {
            return g;
        }
        if !prefix.is_empty() {
            // the prefix matched but the joined entity has no such column
            return default_getter(ty);
        }
    }
    default_getter(ty)
}

fn own_column(table: &str, name: &str) -> Option<Getter> {
    match table {
        "comments" => comments::comment_column(name),
        "downtimes" => comments::downtime_column(name),
        "log" => log::column(name),
        "status" => status::column(name),
        "columns" => misc::columns_column(name),
        "commands" => misc::command_column(name),
        "timeperiods" => misc::timeperiod_column(name),
        "contactgroups" => misc::contactgroup_column(name),
        "crashreports" => misc::crashreport_column(name),
        "labels" => misc::label_column(name),
        _ => None,
    }
}

/// Error raised while resolving a column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoSuchColumn(pub String);

/// Resolve a requested column name. Unknown names yield `Err` (the caller decides
/// whether that is fatal: filters are strict, `Columns:` renders `null`).
pub fn column(table: &str, name: &str) -> Result<Column, NoSuchColumn> {
    match schema::resolve(table, name) {
        Ok(def) => Ok(Column {
            name: name.to_string(),
            canonical: def.name,
            ty: def.ty,
            get: build_getter(table, def),
            rrd: None,
        }),
        Err(stripped) => Err(NoSuchColumn(stripped)),
    }
}

/// A column that renders `null` (unknown column in `Columns:`, like Livestatus).
pub fn null_column(name: &str) -> Column {
    Column { name: name.to_string(), canonical: "", ty: ColumnType::String, get: null_getter(), rrd: None }
}

/// Resolve a dynamic column (`rrddata:TITLE:ARGUMENTS`, `host_rrddata:...` in the
/// service tables). Returns `None` if `name` is not a dynamic column reference.
pub fn dynamic_column(table: &str, name: &str) -> Option<Result<Column, String>> {
    let prefix = schema::name_prefix(table);
    let mut stripped = name;
    if !prefix.is_empty() {
        while let Some(rest) = stripped.strip_prefix(prefix) {
            stripped = rest;
        }
    }
    let (dynamic, rest) = stripped.split_once(':')?;
    Some(rrd_column(table, dynamic, rest))
}

fn rrd_column(table: &str, dynamic: &str, rest: &str) -> Result<Column, String> {
    let entity = match (table, dynamic) {
        ("hosts" | "hostsbygroup", "rrddata") => Entity::Host,
        ("services" | "servicesbygroup" | "servicesbyhostgroup", "rrddata") => Entity::Service,
        ("services" | "servicesbygroup" | "servicesbyhostgroup", "host_rrddata") => Entity::Host,
        _ => return Err(format!("table '{table}' has no dynamic column '{dynamic}'")),
    };
    let (title, arguments) =
        rest.split_once(':').ok_or_else(|| format!("missing separator in dynamic column '{dynamic}'"))?;
    if title.is_empty() {
        return Err(format!("empty column name for dynamic column '{dynamic}'"));
    }
    let args =
        crate::rrd::RrdArgs::parse(arguments).map_err(|e| format!("invalid arguments for column '{dynamic}': {e}"))?;
    let get = match entity {
        Entity::Host => getter(|c, r| {
            r.host(c).map(|h| crate::rrd::object_key(&h.cfg.name, crate::rrd::HOST_SERVICE)).unwrap_or(Value::Null)
        }),
        _ => getter(|c, r| {
            r.service(c)
                .map(|s| crate::rrd::object_key(&c.world.host(s.host).cfg.name, &s.cfg.description))
                .unwrap_or(Value::Null)
        }),
    };
    Ok(Column { name: title.to_string(), canonical: "", ty: ColumnType::List, get, rrd: Some(Arc::new(args)) })
}

/// Data that must be loaded before taking the world lock.
#[derive(Default)]
pub struct Preloaded {
    pub log: Vec<Arc<log::LogChunk>>,
    pub crash_reports: Vec<CrashReport>,
}

/// Prepare table data that requires file I/O.
pub fn preload(table: &str, paths: &Paths, period: (i64, i64)) -> Preloaded {
    let mut p = Preloaded::default();
    match table {
        "log" => p.log = log::load(paths, period),
        "crashreports" => p.crash_reports = misc::load_crash_reports(paths.crash_reports_path.as_deref()),
        _ => {}
    }
    p
}

/// Iterate the rows of a table (respecting authorization). `f` returns `false` to stop.
pub fn for_each_row<'a>(
    table: &str,
    ctx: &Ctx<'a>,
    pre: &'a Preloaded,
    labels: &'a [LabelRow],
    f: &mut dyn FnMut(Row<'a>) -> bool,
) {
    let w = ctx.world;
    macro_rules! each {
        ($iter:expr) => {
            for row in $iter {
                if !f(row) {
                    return;
                }
            }
        };
    }
    match table {
        "hosts" => each!(w.hosts.iter().filter(|h| ctx.host_visible(h)).map(Row::Host)),
        "services" => each!(w.services.iter().filter(|s| ctx.service_visible(s)).map(Row::Service)),
        "hostgroups" => each!(w
            .hostgroups
            .iter()
            .filter(|g| ctx.auth == Auth::All || g.members.iter().any(|h| ctx.host_visible(w.host(*h))))
            .map(Row::HostGroup)),
        "servicegroups" => each!(w
            .servicegroups
            .iter()
            .filter(|g| ctx.auth == Auth::All || g.members.iter().any(|s| ctx.service_visible(w.service(*s))))
            .map(Row::ServiceGroup)),
        "contacts" => each!(w.contacts.iter().map(Row::Contact)),
        "contactgroups" => each!(w.contactgroups.iter().map(Row::ContactGroup)),
        "commands" => {
            let mut cmds: Vec<_> = w.objects.commands.values().collect();
            cmds.sort_by(|a, b| a.name.cmp(&b.name));
            each!(cmds.into_iter().map(Row::Command))
        }
        "timeperiods" => {
            let mut tps: Vec<_> = w.objects.timeperiods.values().collect();
            tps.sort_by(|a, b| a.name.cmp(&b.name));
            each!(tps.into_iter().map(Row::Timeperiod))
        }
        "status" => each!(std::iter::once(Row::Status)),
        "columns" => each!(schema::all_columns().iter().map(Row::Column)),
        "comments" => each!(w.comments.values().filter(|c| ctx.target_visible(c.target)).map(Row::Comment)),
        "downtimes" => each!(w.downtimes.values().filter(|d| ctx.target_visible(d.target)).map(Row::Downtime)),
        "log" => {
            // newest first, like Livestatus
            for chunk in pre.log.iter().rev() {
                for entry in chunk.entries.iter().rev() {
                    if !log_visible(ctx, entry) {
                        continue;
                    }
                    if !f(Row::Log(entry)) {
                        return;
                    }
                }
            }
        }
        "hostsbygroup" => {
            for g in &w.hostgroups {
                each!(g
                    .members
                    .iter()
                    .map(|h| w.host(*h))
                    .filter(|h| ctx.host_visible(h))
                    .map(|h| Row::HostByGroup(h, g)))
            }
        }
        "servicesbygroup" => {
            for g in &w.servicegroups {
                each!(g
                    .members
                    .iter()
                    .map(|s| w.service(*s))
                    .filter(|s| ctx.service_visible(s))
                    .map(|s| Row::ServiceByGroup(s, g)))
            }
        }
        "servicesbyhostgroup" => {
            for g in &w.hostgroups {
                for h in &g.members {
                    each!(w
                        .host(*h)
                        .services
                        .iter()
                        .map(|s| w.service(*s))
                        .filter(|s| ctx.service_visible(s))
                        .map(|s| Row::ServiceByHostGroup(s, g)))
                }
            }
        }
        "crashreports" => each!(pre.crash_reports.iter().map(Row::CrashReport)),
        "labels" => each!(labels.iter().map(Row::Label)),
        // statehist and the Event Console tables are not (yet) provided by corax
        _ => {}
    }
}

fn log_visible(ctx: &Ctx<'_>, e: &LogEntry) -> bool {
    match ctx.auth {
        Auth::All => true,
        Auth::Nobody => false,
        Auth::Contact(c) => {
            if e.host_name.is_empty() {
                return true;
            }
            let w = ctx.world;
            if !e.service_description.is_empty() {
                if let Some(s) = w.find_service(&e.host_name, &e.service_description) {
                    return ctx.service_visible(w.service(s));
                }
            } else if let Some(h) = w.find_host(&e.host_name) {
                return ctx.host_visible(w.host(h));
            }
            // object no longer exists: only show notifications addressed to the user
            w.contact(c).cfg.name == e.contact_name
        }
    }
}

/// Collect the rows of the `labels` table (requires the world).
pub fn collect_labels(table: &str, world: &World) -> Vec<LabelRow> {
    if table == "labels" {
        misc::collect_labels(world)
    } else {
        Vec::new()
    }
}

pub(crate) fn file_exists(p: &Path) -> bool {
    p.is_file()
}

/// Whether the table name exists.
pub fn table_exists(table: &str) -> bool {
    schema::table_columns(table).is_some()
}
