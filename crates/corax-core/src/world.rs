//! The complete in-memory monitoring state ("world") and its construction.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use corax_config::{timeperiod, LoadedConfig, MainConfig, ObjectConfig};

use crate::model::*;

/// Settings derived from the main configuration file.
#[derive(Debug, Clone)]
pub struct Settings {
    pub interval_length: i64,
    pub service_check_timeout: u64,
    pub host_check_timeout: u64,
    pub notification_timeout: u64,
    pub event_handler_timeout: u64,
    pub max_concurrent_checks: usize,
    pub max_service_check_spread: i64,
    pub max_host_check_spread: i64,
    pub low_service_flap_threshold: f64,
    pub high_service_flap_threshold: f64,
    pub low_host_flap_threshold: f64,
    pub high_host_flap_threshold: f64,
    pub service_freshness_check_interval: i64,
    pub host_freshness_check_interval: i64,
    pub additional_freshness_latency: i64,
    pub passive_host_checks_are_soft: bool,
    pub use_aggressive_host_checking: bool,
    pub log_passive_checks: bool,
    pub log_initial_states: bool,
    pub log_external_commands: bool,
    pub log_notifications: bool,
    pub log_service_retries: bool,
    pub log_host_retries: bool,
    pub log_event_handlers: bool,
    pub retain_state_information: bool,
    pub retention_update_interval: i64,
    pub use_retained_program_state: bool,
    pub use_retained_scheduling_info: bool,
    pub check_result_reaper_frequency: f64,
    pub max_check_result_file_age: i64,
    pub enable_environment_macros: bool,
    pub illegal_macro_output_chars: String,
    pub global_host_event_handler: Option<String>,
    pub global_service_event_handler: Option<String>,
    pub cached_host_check_horizon: i64,
    pub log_rotation_method: char,
    /// Dependencies look at soft states too (default: the last hard state counts).
    pub soft_state_dependencies: bool,
}

impl Settings {
    pub fn from_main(m: &MainConfig) -> Self {
        let max_conc = m.i64_or("max_concurrent_checks", 0);
        Settings {
            interval_length: m.i64_or("interval_length", 60).max(1),
            service_check_timeout: m.i64_or("service_check_timeout", 60).max(1) as u64,
            host_check_timeout: m.i64_or("host_check_timeout", 30).max(1) as u64,
            notification_timeout: m.i64_or("notification_timeout", 30).max(1) as u64,
            event_handler_timeout: m.i64_or("event_handler_timeout", 30).max(1) as u64,
            max_concurrent_checks: if max_conc <= 0 { 0 } else { max_conc as usize },
            max_service_check_spread: m.i64_or("max_service_check_spread", 30),
            max_host_check_spread: m.i64_or("max_host_check_spread", 30),
            low_service_flap_threshold: m.f64_or("low_service_flap_threshold", 20.0),
            high_service_flap_threshold: m.f64_or("high_service_flap_threshold", 30.0),
            low_host_flap_threshold: m.f64_or("low_host_flap_threshold", 20.0),
            high_host_flap_threshold: m.f64_or("high_host_flap_threshold", 30.0),
            service_freshness_check_interval: m.i64_or("service_freshness_check_interval", 60),
            host_freshness_check_interval: m.i64_or("host_freshness_check_interval", 60),
            additional_freshness_latency: m.i64_or("additional_freshness_latency", 15),
            passive_host_checks_are_soft: m.bool_or("passive_host_checks_are_soft", false),
            use_aggressive_host_checking: m.bool_or("use_aggressive_host_checking", false),
            log_passive_checks: m.bool_or("log_passive_checks", true),
            log_initial_states: m.bool_or("log_initial_states", false),
            log_external_commands: m.bool_or("log_external_commands", true),
            log_notifications: m.bool_or("log_notifications", true),
            log_service_retries: m.bool_or("log_service_retries", false),
            log_host_retries: m.bool_or("log_host_retries", false),
            log_event_handlers: m.bool_or("log_event_handlers", true),
            retain_state_information: m.bool_or("retain_state_information", true),
            retention_update_interval: m.i64_or("retention_update_interval", 60),
            use_retained_program_state: m.bool_or("use_retained_program_state", true),
            use_retained_scheduling_info: m.bool_or("use_retained_scheduling_info", true),
            check_result_reaper_frequency: m.f64_or("check_result_reaper_frequency", 1.0),
            max_check_result_file_age: m.i64_or("max_check_result_file_age", 3600),
            enable_environment_macros: m.bool_or("enable_environment_macros", false),
            illegal_macro_output_chars: m.str_or("illegal_macro_output_chars", "").to_string(),
            global_host_event_handler: m.get("global_host_event_handler").filter(|s| !s.is_empty()).map(String::from),
            global_service_event_handler: m
                .get("global_service_event_handler")
                .filter(|s| !s.is_empty())
                .map(String::from),
            cached_host_check_horizon: m.i64_or("cached_host_check_horizon", 15),
            log_rotation_method: m.str_or("log_rotation_method", "d").chars().next().unwrap_or('d'),
            soft_state_dependencies: m.bool_or("soft_state_dependencies", false),
        }
    }
}

/// File system locations used by the core and by Livestatus.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub main_config: PathBuf,
    pub log_file: Option<PathBuf>,
    pub log_archive_path: Option<PathBuf>,
    pub command_file: Option<PathBuf>,
    pub check_result_path: Option<PathBuf>,
    pub state_retention_file: Option<PathBuf>,
    pub lock_file: Option<PathBuf>,
    pub temp_path: Option<PathBuf>,
    /// Livestatus socket (last positional argument of the Livestatus broker module).
    pub livestatus_socket: Option<PathBuf>,
    pub pnp_path: Option<PathBuf>,
    pub crash_reports_path: Option<PathBuf>,
    pub mk_inventory_path: Option<PathBuf>,
    pub mk_logwatch_path: Option<PathBuf>,
    pub prediction_path: Option<PathBuf>,
    pub mkeventd_socket: Option<PathBuf>,
    /// rrdcached socket used to read historic metrics (`rrddata` Livestatus columns).
    pub rrdcached_socket: Option<PathBuf>,
    pub edition: String,
    /// Checkmk version of the OMD site (e.g. `2.5.0p20`), reported as `livestatus_version`.
    pub checkmk_version: Option<String>,
    /// npcdmod: `perfdata_file`, `perfdata_spool_dir`, `perfdata_file_processing_interval`.
    pub npcd_perfdata_file: Option<PathBuf>,
    pub npcd_spool_dir: Option<PathBuf>,
    pub npcd_processing_interval: i64,
}

impl Paths {
    pub fn from_main(m: &MainConfig) -> Self {
        let mut p = Paths {
            main_config: m.path.clone(),
            log_file: m.path("log_file"),
            log_archive_path: m.path("log_archive_path"),
            command_file: m.path("command_file"),
            check_result_path: m.path("check_result_path"),
            state_retention_file: m.path("state_retention_file"),
            lock_file: m.path("lock_file"),
            temp_path: m.path("temp_path"),
            edition: "community".into(),
            npcd_processing_interval: 15,
            ..Default::default()
        };
        if let Some(ls) = m.livestatus_module() {
            p.livestatus_socket = ls.positional.last().map(PathBuf::from);
            let opt = |k: &str| ls.options.get(k).filter(|v| !v.is_empty()).map(PathBuf::from);
            p.pnp_path = opt("pnp_path");
            p.crash_reports_path = opt("crash_reports_path");
            p.mk_inventory_path = opt("mk_inventory_path");
            p.mk_logwatch_path = opt("mk_logwatch_path");
            p.prediction_path = opt("prediction_path");
            p.mkeventd_socket = opt("mkeventd_status_socket");
            if let Some(e) = ls.options.get("edition") {
                p.edition = e.clone();
            }
        }
        if let Some(npcd) = m.npcd_module() {
            if let Some(cfg) = npcd.options.get("config_file") {
                if let Ok(text) = std::fs::read_to_string(cfg) {
                    for line in text.lines() {
                        let Some((k, v)) = line.split_once('=') else { continue };
                        let (k, v) = (k.trim(), v.trim());
                        match k {
                            "perfdata_file" => p.npcd_perfdata_file = Some(PathBuf::from(v)),
                            "perfdata_spool_dir" => p.npcd_spool_dir = Some(PathBuf::from(v)),
                            "perfdata_file_processing_interval" => {
                                p.npcd_processing_interval = v.parse().unwrap_or(15).max(1)
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        p.checkmk_version = omd_root(&p).and_then(|root| checkmk_version(&root));
        // OMD convention: both sockets live next to the Livestatus socket
        // ($OMD_ROOT/tmp/run/), exactly like the original Livestatus derives them.
        if let Some(dir) = p.livestatus_socket.as_ref().and_then(|s| s.parent()).map(|d| d.to_path_buf()) {
            if p.mkeventd_socket.is_none() {
                p.mkeventd_socket = Some(dir.join("mkeventd/status"));
            }
            p.rrdcached_socket = Some(dir.join("rrdcached.sock"));
        }
        p
    }
}

/// `$OMD_ROOT`, from the environment or derived from `<root>/tmp/run/live`.
fn omd_root(p: &Paths) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("OMD_ROOT") {
        return Some(PathBuf::from(root));
    }
    let sock = p.livestatus_socket.as_ref()?;
    let root = sock.parent()?.parent()?.parent()?;
    root.join("version").exists().then(|| root.to_path_buf())
}

/// `$OMD_ROOT/version` links to `../../versions/2.5.0p20.community`.
fn checkmk_version(root: &std::path::Path) -> Option<String> {
    let target = std::fs::read_link(root.join("version")).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    let version = match name.rsplit_once('.') {
        Some((v, edition)) if edition.chars().all(|c| c.is_ascii_alphabetic()) => v.to_string(),
        _ => name,
    };
    (!version.is_empty()).then_some(version)
}

/// Group with resolved member ids.
#[derive(Debug, Clone)]
pub struct HostGroup {
    pub cfg: corax_config::GroupConfig,
    pub members: Vec<HostId>,
}

#[derive(Debug, Clone)]
pub struct ServiceGroup {
    pub cfg: corax_config::ServiceGroupConfig,
    pub members: Vec<ServiceId>,
}

#[derive(Debug, Clone)]
pub struct ContactGroup {
    pub cfg: corax_config::GroupConfig,
    pub members: Vec<ContactId>,
}

/// The complete monitoring state. Protected by a `RwLock` in [`crate::Shared`]; only the
/// engine task writes, Livestatus readers take short read locks.
/// One edge of a host or service dependency, seen from the dependent object.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub master: Target,
    /// Bit `n` set: the dependency fails while the master is in state `n`.
    pub fail_states: u8,
    /// The dependency fails while the master has not been checked yet.
    pub fail_on_pending: bool,
    /// Also fails if a dependency of the master fails.
    pub inherits_parent: bool,
    pub period: Option<String>,
}

/// What an object depends on.
#[derive(Debug, Clone, Default)]
pub struct Dependencies {
    /// No problem and recovery notifications while one of these fails.
    pub notification: Vec<Dependency>,
    /// No scheduled active checks while one of these fails.
    pub execution: Vec<Dependency>,
}

/// Parse failure criteria (`o,w,u,c,p` for services, `o,d,u,p` for hosts, `a`, `n`).
fn failure_criteria(criteria: &str, is_host: bool) -> (u8, bool) {
    let mut states = 0u8;
    let mut pending = false;
    for opt in criteria.split(',').map(str::trim) {
        match (opt, is_host) {
            ("o", _) => states |= 1 << 0,
            ("w", false) | ("d", true) => states |= 1 << 1,
            ("c", false) | ("u", true) => states |= 1 << 2,
            ("u", false) => states |= 1 << 3,
            ("p", _) => pending = true,
            ("a", _) => return (if is_host { 0b0111 } else { 0b1111 }, true),
            _ => {}
        }
    }
    (states, pending)
}

/// Nesting limit of `inherits_parent` (dependency loops end here).
const MAX_DEPENDENCY_DEPTH: u8 = 16;

#[derive(Debug)]
pub struct World {
    pub objects: Arc<ObjectConfig>,
    pub settings: Settings,
    pub resources: HashMap<String, String>,
    pub hosts: Vec<Host>,
    pub services: Vec<Service>,
    pub contacts: Vec<Contact>,
    pub hostgroups: Vec<HostGroup>,
    pub servicegroups: Vec<ServiceGroup>,
    pub contactgroups: Vec<ContactGroup>,
    pub host_by_name: HashMap<String, HostId>,
    /// Per host: service description → id.
    pub service_by_name: Vec<HashMap<String, ServiceId>>,
    pub contact_by_name: HashMap<String, ContactId>,
    /// Indexed by host id.
    pub host_dependencies: Vec<Dependencies>,
    /// Indexed by service id.
    pub service_dependencies: Vec<Dependencies>,
    pub comments: BTreeMap<u64, Comment>,
    pub downtimes: BTreeMap<u64, Downtime>,
    pub program: ProgramState,
    pub counters: Counters,
    /// `program_version` reported via Livestatus.
    pub version: String,
    /// Incremented on every configuration reload; object ids are only valid within an epoch.
    pub epoch: u64,
}

impl World {
    pub fn new(cfg: &LoadedConfig, now: i64) -> Self {
        let objects = Arc::new(cfg.objects.clone());
        let settings = Settings::from_main(&cfg.main);
        let main = &cfg.main;

        let contacts: Vec<Contact> = objects
            .contacts
            .iter()
            .enumerate()
            .map(|(i, c)| Contact {
                id: ContactId(i as u32),
                host_notifications_enabled: c.host_notifications_enabled,
                service_notifications_enabled: c.service_notifications_enabled,
                cfg: c.clone(),
                last_host_notification: 0,
                last_service_notification: 0,
                modified_attributes: 0,
            })
            .collect();
        let contact_by_name: HashMap<String, ContactId> = contacts.iter().map(|c| (c.cfg.name.clone(), c.id)).collect();
        let contactgroups: Vec<ContactGroup> = objects
            .contactgroups
            .iter()
            .map(|g| ContactGroup {
                cfg: g.clone(),
                members: g.members.iter().filter_map(|m| contact_by_name.get(m).copied()).collect(),
            })
            .collect();
        let cg_by_name: HashMap<&str, &ContactGroup> = contactgroups.iter().map(|g| (g.cfg.name.as_str(), g)).collect();
        let resolve_contacts = |c: &corax_config::CheckableConfig| -> Vec<ContactId> {
            let mut out: Vec<ContactId> = c.contacts.iter().filter_map(|n| contact_by_name.get(n).copied()).collect();
            for g in &c.contact_groups {
                if let Some(g) = cg_by_name.get(g.as_str()) {
                    out.extend(g.members.iter().copied());
                }
            }
            out.sort();
            out.dedup();
            out
        };

        let mut hosts: Vec<Host> = objects
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| Host {
                id: HostId(i as u32),
                contacts: resolve_contacts(&h.common),
                st: CheckState::new(&h.common),
                cfg: h.clone(),
                services: Vec::new(),
                parents: Vec::new(),
                children: Vec::new(),
            })
            .collect();
        let host_by_name: HashMap<String, HostId> = hosts.iter().map(|h| (h.cfg.name.clone(), h.id)).collect();
        for i in 0..hosts.len() {
            let parents: Vec<HostId> =
                hosts[i].cfg.parents.iter().filter_map(|p| host_by_name.get(p).copied()).collect();
            for p in &parents {
                hosts[p.0 as usize].children.push(HostId(i as u32));
            }
            hosts[i].parents = parents;
        }

        let mut service_by_name: Vec<HashMap<String, ServiceId>> = vec![HashMap::new(); hosts.len()];
        let mut services = Vec::with_capacity(objects.services.len());
        for s in &objects.services {
            let Some(&hid) = host_by_name.get(&s.host_name) else { continue };
            let id = ServiceId(services.len() as u32);
            hosts[hid.0 as usize].services.push(id);
            service_by_name[hid.0 as usize].insert(s.description.clone(), id);
            services.push(Service {
                id,
                host: hid,
                contacts: resolve_contacts(&s.common),
                st: CheckState::new(&s.common),
                cfg: s.clone(),
            });
        }

        let hostgroups = objects
            .hostgroups
            .iter()
            .map(|g| HostGroup {
                cfg: g.clone(),
                members: g.members.iter().filter_map(|m| host_by_name.get(m).copied()).collect(),
            })
            .collect();
        let servicegroups = objects
            .servicegroups
            .iter()
            .map(|g| ServiceGroup {
                cfg: g.clone(),
                members: g
                    .members
                    .iter()
                    .filter_map(|(h, s)| {
                        let hid = host_by_name.get(h)?;
                        service_by_name[hid.0 as usize].get(s).copied()
                    })
                    .collect(),
            })
            .collect();

        let mut host_dependencies = vec![Dependencies::default(); hosts.len()];
        let mut service_dependencies = vec![Dependencies::default(); services.len()];
        let edges = |master: Target, notification: &str, execution: &str, inherits: bool, period: &Option<String>| {
            let is_host = matches!(master, Target::Host(_));
            [notification, execution].map(|criteria| {
                let (fail_states, fail_on_pending) = failure_criteria(criteria, is_host);
                (fail_states != 0 || fail_on_pending).then(|| Dependency {
                    master,
                    fail_states,
                    fail_on_pending,
                    inherits_parent: inherits,
                    period: period.clone(),
                })
            })
        };
        for d in &objects.host_dependencies {
            let (Some(master), Some(dependent)) =
                (host_by_name.get(&d.host_name), host_by_name.get(&d.dependent_host_name))
            else {
                continue;
            };
            let [notification, execution] = edges(
                Target::Host(*master),
                &d.notification_failure_criteria,
                &d.execution_failure_criteria,
                d.inherits_parent,
                &d.dependency_period,
            );
            let deps = &mut host_dependencies[dependent.0 as usize];
            deps.notification.extend(notification);
            deps.execution.extend(execution);
        }
        for d in &objects.service_dependencies {
            let find = |host: &str, service: &str| {
                let hid = host_by_name.get(host)?;
                service_by_name[hid.0 as usize].get(service).copied()
            };
            let (Some(master), Some(dependent)) = (
                find(&d.host_name, &d.service_description),
                find(&d.dependent_host_name, &d.dependent_service_description),
            ) else {
                continue;
            };
            let [notification, execution] = edges(
                Target::Service(master),
                &d.notification_failure_criteria,
                &d.execution_failure_criteria,
                d.inherits_parent,
                &d.dependency_period,
            );
            let deps = &mut service_dependencies[dependent.0 as usize];
            deps.notification.extend(notification);
            deps.execution.extend(execution);
        }

        let program = ProgramState {
            program_start: now,
            pid: std::process::id(),
            enable_notifications: main.bool_or("enable_notifications", true),
            execute_service_checks: main.bool_or("execute_service_checks", true),
            accept_passive_service_checks: main.bool_or("accept_passive_service_checks", true),
            execute_host_checks: main.bool_or("execute_host_checks", true),
            accept_passive_host_checks: main.bool_or("accept_passive_host_checks", true),
            enable_event_handlers: main.bool_or("enable_event_handlers", true),
            enable_flap_detection: main.bool_or("enable_flap_detection", false),
            process_performance_data: main.bool_or("process_performance_data", false),
            check_service_freshness: main.bool_or("check_service_freshness", true),
            check_host_freshness: main.bool_or("check_host_freshness", false),
            obsess_over_services: main.bool_or("obsess_over_services", false),
            obsess_over_hosts: main.bool_or("obsess_over_hosts", false),
            last_command_check: 0,
            last_log_rotation: 0,
            next_comment_id: 1,
            next_downtime_id: 1,
            next_event_id: 1,
            next_problem_id: 1,
            next_notification_id: 1,
            modified_host_attributes: 0,
            modified_service_attributes: 0,
        };

        World {
            settings,
            resources: cfg.resources.user.clone(),
            hosts,
            services,
            contacts,
            hostgroups,
            servicegroups,
            contactgroups,
            host_by_name,
            service_by_name,
            contact_by_name,
            host_dependencies,
            service_dependencies,
            comments: BTreeMap::new(),
            downtimes: BTreeMap::new(),
            program,
            counters: Counters::default(),
            // Must not contain spaces: the GUI uses it in file names.
            version: format!("corax-{}", env!("CARGO_PKG_VERSION")),
            epoch: 0,
            objects,
        }
    }

    /// Names identifying a target independent of the epoch.
    pub fn target_names(&self, t: Target) -> (String, Option<String>) {
        match t {
            Target::Host(h) => (self.host(h).cfg.name.clone(), None),
            Target::Service(s) => {
                let svc = self.service(s);
                (self.host(svc.host).cfg.name.clone(), Some(svc.cfg.description.clone()))
            }
        }
    }

    pub fn host(&self, id: HostId) -> &Host {
        &self.hosts[id.0 as usize]
    }

    pub fn host_mut(&mut self, id: HostId) -> &mut Host {
        &mut self.hosts[id.0 as usize]
    }

    pub fn service(&self, id: ServiceId) -> &Service {
        &self.services[id.0 as usize]
    }

    pub fn service_mut(&mut self, id: ServiceId) -> &mut Service {
        &mut self.services[id.0 as usize]
    }

    pub fn find_host(&self, name: &str) -> Option<HostId> {
        self.host_by_name.get(name).copied()
    }

    pub fn find_service(&self, host: &str, desc: &str) -> Option<ServiceId> {
        let hid = self.find_host(host)?;
        self.service_by_name[hid.0 as usize].get(desc).copied()
    }

    pub fn contact(&self, id: ContactId) -> &Contact {
        &self.contacts[id.0 as usize]
    }

    pub fn state(&self, t: Target) -> &CheckState {
        match t {
            Target::Host(h) => &self.host(h).st,
            Target::Service(s) => &self.service(s).st,
        }
    }

    pub fn state_mut(&mut self, t: Target) -> &mut CheckState {
        match t {
            Target::Host(h) => &mut self.host_mut(h).st,
            Target::Service(s) => &mut self.service_mut(s).st,
        }
    }

    pub fn common(&self, t: Target) -> &corax_config::CheckableConfig {
        match t {
            Target::Host(h) => &self.host(h).cfg.common,
            Target::Service(s) => &self.service(s).cfg.common,
        }
    }

    /// Host of a target (the host itself for host targets).
    pub fn host_of(&self, t: Target) -> HostId {
        match t {
            Target::Host(h) => h,
            Target::Service(s) => self.service(s).host,
        }
    }

    /// Human readable `host` or `host;service` used in log lines.
    pub fn target_name(&self, t: Target) -> String {
        match t {
            Target::Host(h) => self.host(h).cfg.name.clone(),
            Target::Service(s) => {
                let svc = self.service(s);
                format!("{};{}", self.host(svc.host).cfg.name, svc.cfg.description)
            }
        }
    }

    /// Whether a notification (or execution) dependency of `t` fails at `now`.
    pub fn dependency_failed(&self, t: Target, execution: bool, now: i64) -> bool {
        self.dependency_failed_at_depth(t, execution, now, 0)
    }

    fn dependency_failed_at_depth(&self, t: Target, execution: bool, now: i64, depth: u8) -> bool {
        if depth >= MAX_DEPENDENCY_DEPTH {
            return false;
        }
        let deps = match t {
            Target::Host(h) => self.host_dependencies.get(h.0 as usize),
            Target::Service(s) => self.service_dependencies.get(s.0 as usize),
        };
        let Some(deps) = deps else { return false };
        let edges = if execution { &deps.execution } else { &deps.notification };
        edges.iter().any(|d| {
            if d.period.is_some() && !timeperiod::is_active(&self.objects.timeperiods, d.period.as_deref(), now) {
                return false;
            }
            let st = self.state(d.master);
            let state = if self.settings.soft_state_dependencies { st.current_state } else { st.hard_state() };
            d.fail_states & (1 << state.min(7)) != 0
                || (d.fail_on_pending && state == 0 && !st.has_been_checked)
                || (d.inherits_parent && self.dependency_failed_at_depth(d.master, execution, now, depth + 1))
        })
    }

    /// Check interval of a target in seconds.
    pub fn check_interval_secs(&self, t: Target) -> i64 {
        (self.common(t).check_interval * self.settings.interval_length as f64).round() as i64
    }

    pub fn retry_interval_secs(&self, t: Target) -> i64 {
        (self.common(t).retry_interval * self.settings.interval_length as f64).round() as i64
    }

    pub fn is_contact_of(&self, contact: ContactId, t: Target) -> bool {
        match t {
            Target::Host(h) => self.host(h).contacts.contains(&contact),
            Target::Service(s) => self.service(s).contacts.contains(&contact),
        }
    }

    pub fn comments_of(&self, t: Target) -> impl Iterator<Item = &Comment> {
        self.comments.values().filter(move |c| c.target == t)
    }

    pub fn downtimes_of(&self, t: Target) -> impl Iterator<Item = &Downtime> {
        self.downtimes.values().filter(move |d| d.target == t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkmk_version_from_omd_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("../../versions/2.5.0p20.community", dir.path().join("version")).unwrap();
        assert_eq!(checkmk_version(dir.path()).as_deref(), Some("2.5.0p20"));
    }

    #[test]
    fn sockets_derived_from_livestatus_socket() {
        let main = MainConfig::parse_str(
            "broker_module=/omd/sites/x/lib/mk-livestatus/livestatus.o pnp_path=/p /omd/sites/x/tmp/run/live\n",
            std::path::Path::new("/omd/sites/x/tmp/nagios/nagios.cfg"),
        )
        .unwrap();
        let p = Paths::from_main(&main);
        assert_eq!(p.rrdcached_socket, Some(PathBuf::from("/omd/sites/x/tmp/run/rrdcached.sock")));
        assert_eq!(p.mkeventd_socket, Some(PathBuf::from("/omd/sites/x/tmp/run/mkeventd/status")));
        assert_eq!(p.pnp_path, Some(PathBuf::from("/p")));
    }
}
