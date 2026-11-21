//! Template resolution, object expansion, group membership and validation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::lexer::{parse_object_text, RawObject};
use crate::main_cfg::{MainConfig, Resources};
use crate::timeperiod::Timeperiod;
use crate::types::*;
use crate::{ConfigError, Diagnostic, Severity};

/// Everything needed to run the core.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub main: MainConfig,
    pub resources: Resources,
    pub objects: ObjectConfig,
    pub warnings: Vec<Diagnostic>,
}

/// Load the main configuration and all referenced object files.
pub fn load(main_cfg: &Path) -> Result<LoadedConfig, ConfigError> {
    let main = MainConfig::parse_file(main_cfg)?;
    let resources = Resources::load(&main.resource_files)?;
    let mut files = main.cfg_files.clone();
    for dir in &main.cfg_dirs {
        collect_cfg_files(dir, &mut files)?;
    }
    let (objects, warnings) = load_objects(&files)?;
    Ok(LoadedConfig { main, resources, objects, warnings })
}

/// Recursively collect `*.cfg` files (sorted, like Nagios' readdir + sort in OMD).
fn collect_cfg_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ConfigError> {
    let io = |source| ConfigError::Io { path: dir.to_path_buf(), source };
    let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(io)?.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_cfg_files(&path, out)?;
        } else if name.ends_with(".cfg") {
            out.push(path);
        }
    }
    Ok(())
}

/// Parse and resolve the given object files.
pub fn load_objects(files: &[PathBuf]) -> Result<(ObjectConfig, Vec<Diagnostic>), ConfigError> {
    let mut raw = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file).map_err(|source| ConfigError::Io { path: file.clone(), source })?;
        raw.extend(parse_object_text(&text, file)?);
    }
    let mut r = Resolver::new(raw);
    let objects = r.build();
    let errors: Vec<&Diagnostic> = r.diags.iter().filter(|d| d.severity == Severity::Error).collect();
    if !errors.is_empty() {
        let details = errors.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
        return Err(ConfigError::Invalid { count: errors.len(), details });
    }
    Ok((objects, r.diags))
}

/// Resolved attribute map of one object.
type Attrs = HashMap<String, String>;

struct Resolver {
    by_kind: HashMap<String, Vec<RawObject>>,
    diags: Vec<Diagnostic>,
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn flag(a: &Attrs, key: &str, default: bool) -> bool {
    a.get(key).map(|v| v.trim() != "0").unwrap_or(default)
}

fn num(a: &Attrs, key: &str, default: f64) -> f64 {
    a.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn text(a: &Attrs, key: &str) -> String {
    a.get(key).cloned().unwrap_or_default()
}

fn opt_text(a: &Attrs, key: &str) -> Option<String> {
    a.get(key).filter(|v| !v.is_empty()).cloned()
}

fn custom_vars(a: &Attrs, order: &[String]) -> Vec<(String, String)> {
    order
        .iter()
        .filter_map(|k| {
            let name = k.strip_prefix('_')?;
            a.get(k).map(|v| (name.to_ascii_uppercase(), v.clone()))
        })
        .collect()
}

impl Resolver {
    fn new(raw: Vec<RawObject>) -> Self {
        let mut by_kind: HashMap<String, Vec<RawObject>> = HashMap::new();
        for obj in raw {
            by_kind.entry(obj.kind.clone()).or_default().push(obj);
        }
        Self { by_kind, diags: Vec::new() }
    }

    fn err(&mut self, message: String, loc: Option<(&Path, usize)>) {
        self.diags.push(Diagnostic {
            severity: Severity::Error,
            message,
            location: loc.map(|(p, l)| (p.to_path_buf(), l)),
        });
    }

    fn warn(&mut self, message: String, loc: Option<(&Path, usize)>) {
        self.diags.push(Diagnostic {
            severity: Severity::Warning,
            message,
            location: loc.map(|(p, l)| (p.to_path_buf(), l)),
        });
    }

    /// Resolve all registered objects of `kind`. Returns `(attrs, key order, raw object)`.
    fn resolve_kind(&mut self, kind: &str) -> Vec<(Attrs, Vec<String>, RawObject)> {
        let objects = self.by_kind.remove(kind).unwrap_or_default();
        let templates: HashMap<String, usize> =
            objects.iter().enumerate().filter_map(|(i, o)| o.get("name").map(|n| (n.to_string(), i))).collect();
        let mut cache: HashMap<usize, (Attrs, Vec<String>)> = HashMap::new();
        let mut out = Vec::new();
        for idx in 0..objects.len() {
            if objects[idx].get("register").map(str::trim) == Some("0") {
                continue;
            }
            let mut stack = Vec::new();
            let (mut attrs, order) = self.resolve_one(idx, &objects, &templates, &mut cache, &mut stack);
            attrs.retain(|_, v| v != "null");
            for v in attrs.values_mut() {
                if let Some(stripped) = v.strip_prefix('+') {
                    *v = stripped.to_string();
                }
            }
            out.push((attrs, order, objects[idx].clone()));
        }
        out
    }

    fn resolve_one(
        &mut self,
        idx: usize,
        objects: &[RawObject],
        templates: &HashMap<String, usize>,
        cache: &mut HashMap<usize, (Attrs, Vec<String>)>,
        stack: &mut Vec<usize>,
    ) -> (Attrs, Vec<String>) {
        if let Some(hit) = cache.get(&idx) {
            return hit.clone();
        }
        let obj = &objects[idx];
        if stack.contains(&idx) {
            self.err(format!("circular template inheritance in {} definition", obj.kind), Some((&obj.file, obj.line)));
            return (Attrs::new(), Vec::new());
        }
        stack.push(idx);
        let mut attrs: Attrs = Attrs::new();
        let mut order: Vec<String> = Vec::new();
        for (k, v) in &obj.attrs {
            if !attrs.contains_key(k) {
                order.push(k.clone());
            }
            attrs.insert(k.clone(), v.clone());
        }
        if let Some(uses) = obj.get("use") {
            for tname in split_list(uses) {
                let Some(&tidx) = templates.get(&tname) else {
                    self.err(
                        format!("template '{tname}' specified in {} definition could not be found", obj.kind),
                        Some((&obj.file, obj.line)),
                    );
                    continue;
                };
                let (tattrs, torder) = self.resolve_one(tidx, objects, templates, cache, stack);
                for key in torder {
                    if matches!(key.as_str(), "name" | "register" | "use") {
                        continue;
                    }
                    let tval = &tattrs[&key];
                    match attrs.get_mut(&key) {
                        None => {
                            order.push(key.clone());
                            attrs.insert(key, tval.clone());
                        }
                        Some(own) => {
                            // additive inheritance: `+value` appends to the inherited list
                            if let Some(add) = own.strip_prefix('+') {
                                let inherited = tval.trim_start_matches('+');
                                *own =
                                    if inherited.is_empty() { add.to_string() } else { format!("{inherited},{add}") };
                            }
                        }
                    }
                }
            }
        }
        stack.pop();
        cache.insert(idx, (attrs.clone(), order.clone()));
        (attrs, order)
    }

    fn checkable(&mut self, a: &Attrs, order: &[String], raw: &RawObject, is_host: bool) -> CheckableConfig {
        let check_interval = a.get("check_interval").or_else(|| a.get("normal_check_interval"));
        let retry_interval = a.get("retry_interval").or_else(|| a.get("retry_check_interval"));
        let max_attempts = a.get("max_check_attempts").and_then(|v| v.trim().parse::<u32>().ok());
        let max_check_attempts = match max_attempts {
            Some(n) if n > 0 => n,
            _ => {
                let what = if is_host { "host" } else { "service" };
                self.err(format!("{what} has no valid max_check_attempts"), Some((&raw.file, raw.line)));
                1
            }
        };
        CheckableConfig {
            display_name: text(a, "display_name"),
            check_command: opt_text(a, "check_command").map(|c| CommandCall::parse(&c)),
            check_interval: check_interval.and_then(|v| v.trim().parse().ok()).unwrap_or(5.0),
            retry_interval: retry_interval.and_then(|v| v.trim().parse().ok()).unwrap_or(1.0),
            max_check_attempts,
            check_period: opt_text(a, "check_period"),
            notification_period: opt_text(a, "notification_period"),
            notification_interval: num(a, "notification_interval", 30.0),
            first_notification_delay: num(a, "first_notification_delay", 0.0),
            notification_options: a
                .get("notification_options")
                .map(|v| StateOptions::parse(v, is_host))
                .unwrap_or(if is_host { StateOptions::ALL_HOST } else { StateOptions::ALL_SERVICE }),
            notifications_enabled: flag(a, "notifications_enabled", true),
            active_checks_enabled: flag(a, "active_checks_enabled", true),
            passive_checks_enabled: flag(a, "passive_checks_enabled", true),
            check_freshness: flag(a, "check_freshness", false),
            freshness_threshold: num(a, "freshness_threshold", 0.0) as i64,
            event_handler: opt_text(a, "event_handler").map(|c| CommandCall::parse(&c)),
            event_handler_enabled: flag(a, "event_handler_enabled", true),
            flap_detection_enabled: flag(a, "flap_detection_enabled", true),
            low_flap_threshold: num(a, "low_flap_threshold", 0.0),
            high_flap_threshold: num(a, "high_flap_threshold", 0.0),
            process_perf_data: flag(a, "process_perf_data", true),
            obsess: flag(a, if is_host { "obsess_over_host" } else { "obsess_over_service" }, true),
            contacts: a.get("contacts").map(|v| split_list(v)).unwrap_or_default(),
            contact_groups: a.get("contact_groups").map(|v| split_list(v)).unwrap_or_default(),
            notes: text(a, "notes"),
            notes_url: text(a, "notes_url"),
            action_url: text(a, "action_url"),
            icon_image: text(a, "icon_image"),
            icon_image_alt: text(a, "icon_image_alt"),
            custom_vars: custom_vars(a, order),
            file: raw.file.clone(),
            line: raw.line,
        }
    }

    fn build(&mut self) -> ObjectConfig {
        let mut cfg = ObjectConfig::default();

        for (a, _, raw) in self.resolve_kind("command") {
            match (opt_text(&a, "command_name"), a.get("command_line")) {
                (Some(name), Some(line)) => {
                    cfg.commands.insert(name.clone(), CommandConfig { name, line: line.clone() });
                }
                _ => self
                    .err("command definition without command_name or command_line".into(), Some((&raw.file, raw.line))),
            }
        }

        for (a, order, raw) in self.resolve_kind("timeperiod") {
            let mut tp = Timeperiod::default();
            // Exceptions share their first word (`day 1 …`, `day 15 …`), so the lines of
            // the object itself are taken one by one; templates only add what is missing.
            let own: HashSet<&str> = raw.attrs.iter().map(|(k, _)| k.as_str()).collect();
            for key in order.iter().filter(|k| !own.contains(k.as_str())) {
                tp.add_attr(key, &a[key]);
            }
            for (key, value) in raw.attrs.iter().filter(|(_, v)| v != "null") {
                tp.add_attr(key, value);
            }
            if tp.name.is_empty() {
                self.err("timeperiod without timeperiod_name".into(), Some((&raw.file, raw.line)));
                continue;
            }
            for u in std::mem::take(&mut tp.unsupported) {
                self.warn(
                    format!("timeperiod '{}': unsupported entry '{u}' ignored", tp.name),
                    Some((&raw.file, raw.line)),
                );
            }
            cfg.timeperiods.insert(tp.name.clone(), tp);
        }

        // --- groups (definitions first, membership merged below)
        let mut hostgroups: Vec<GroupConfig> = Vec::new();
        for (a, _, raw) in self.resolve_kind("hostgroup") {
            let Some(name) = opt_text(&a, "hostgroup_name") else {
                self.err("hostgroup without hostgroup_name".into(), Some((&raw.file, raw.line)));
                continue;
            };
            hostgroups.push(GroupConfig {
                alias: opt_text(&a, "alias").unwrap_or_else(|| name.clone()),
                name,
                notes: text(&a, "notes"),
                notes_url: text(&a, "notes_url"),
                action_url: text(&a, "action_url"),
                members: a.get("members").map(|v| split_list(v)).unwrap_or_default(),
            });
        }
        let mut servicegroups: Vec<ServiceGroupConfig> = Vec::new();
        for (a, _, raw) in self.resolve_kind("servicegroup") {
            let Some(name) = opt_text(&a, "servicegroup_name") else {
                self.err("servicegroup without servicegroup_name".into(), Some((&raw.file, raw.line)));
                continue;
            };
            let list = a.get("members").map(|v| split_list(v)).unwrap_or_default();
            let members = list.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect();
            servicegroups.push(ServiceGroupConfig {
                alias: opt_text(&a, "alias").unwrap_or_else(|| name.clone()),
                name,
                notes: text(&a, "notes"),
                notes_url: text(&a, "notes_url"),
                action_url: text(&a, "action_url"),
                members,
            });
        }
        let mut contactgroups: Vec<GroupConfig> = Vec::new();
        for (a, _, raw) in self.resolve_kind("contactgroup") {
            let Some(name) = opt_text(&a, "contactgroup_name") else {
                self.err("contactgroup without contactgroup_name".into(), Some((&raw.file, raw.line)));
                continue;
            };
            contactgroups.push(GroupConfig {
                alias: opt_text(&a, "alias").unwrap_or_else(|| name.clone()),
                name,
                notes: String::new(),
                notes_url: String::new(),
                action_url: String::new(),
                members: a.get("members").map(|v| split_list(v)).unwrap_or_default(),
            });
        }

        // --- contacts
        for (a, order, raw) in self.resolve_kind("contact") {
            let Some(name) = opt_text(&a, "contact_name") else {
                self.err("contact without contact_name".into(), Some((&raw.file, raw.line)));
                continue;
            };
            let cmds = |key: &str| -> Vec<CommandCall> {
                a.get(key).map(|v| split_list(v).iter().map(|c| CommandCall::parse(c)).collect()).unwrap_or_default()
            };
            cfg.contacts.push(ContactConfig {
                alias: opt_text(&a, "alias").unwrap_or_else(|| name.clone()),
                email: text(&a, "email"),
                pager: text(&a, "pager"),
                addresses: (1..=6).map(|i| text(&a, &format!("address{i}"))).collect(),
                host_notification_period: opt_text(&a, "host_notification_period"),
                service_notification_period: opt_text(&a, "service_notification_period"),
                host_notification_options: a
                    .get("host_notification_options")
                    .map(|v| StateOptions::parse(v, true))
                    .unwrap_or(StateOptions::ALL_HOST),
                service_notification_options: a
                    .get("service_notification_options")
                    .map(|v| StateOptions::parse(v, false))
                    .unwrap_or(StateOptions::ALL_SERVICE),
                host_notification_commands: cmds("host_notification_commands"),
                service_notification_commands: cmds("service_notification_commands"),
                host_notifications_enabled: flag(&a, "host_notifications_enabled", true),
                service_notifications_enabled: flag(&a, "service_notifications_enabled", true),
                can_submit_commands: flag(&a, "can_submit_commands", true),
                contactgroups: a.get("contactgroups").map(|v| split_list(v)).unwrap_or_default(),
                custom_vars: custom_vars(&a, &order),
                name,
            });
        }

        // --- hosts
        let mut host_names: HashSet<String> = HashSet::new();
        for (a, order, raw) in self.resolve_kind("host") {
            let Some(name) = opt_text(&a, "host_name") else {
                self.err("host definition without host_name".into(), Some((&raw.file, raw.line)));
                continue;
            };
            if !host_names.insert(name.clone()) {
                self.err(format!("duplicate definition of host '{name}'"), Some((&raw.file, raw.line)));
                continue;
            }
            let mut common = self.checkable(&a, &order, &raw, true);
            if common.display_name.is_empty() {
                common.display_name = name.clone();
            }
            cfg.hosts.push(HostConfig {
                alias: opt_text(&a, "alias").unwrap_or_else(|| name.clone()),
                address: opt_text(&a, "address").unwrap_or_else(|| name.clone()),
                parents: a.get("parents").map(|v| split_list(v)).unwrap_or_default(),
                hostgroups: a.get("hostgroups").map(|v| split_list(v)).unwrap_or_default(),
                statusmap_image: text(&a, "statusmap_image"),
                common,
                name,
            });
        }

        // host group membership from both sides
        let hg_index: HashMap<String, usize> =
            hostgroups.iter().enumerate().map(|(i, g)| (g.name.clone(), i)).collect();
        for host in &mut cfg.hosts {
            for g in &host.hostgroups {
                match hg_index.get(g) {
                    Some(&i) => {
                        if !hostgroups[i].members.contains(&host.name) {
                            hostgroups[i].members.push(host.name.clone());
                        }
                    }
                    None => self.diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!("host '{}' refers to unknown hostgroup '{g}'", host.name),
                        location: Some((host.common.file.clone(), host.common.line)),
                    }),
                }
            }
        }
        let host_idx: HashMap<String, usize> = cfg.hosts.iter().enumerate().map(|(i, h)| (h.name.clone(), i)).collect();
        for g in &mut hostgroups {
            if g.members.iter().any(|m| m == "*") {
                g.members = cfg.hosts.iter().map(|h| h.name.clone()).collect();
            }
            g.members.retain(|m| host_idx.contains_key(m));
            for m in &g.members {
                let h = &mut cfg.hosts[host_idx[m]];
                if !h.hostgroups.contains(&g.name) {
                    h.hostgroups.push(g.name.clone());
                }
            }
        }

        // --- services (with host list expansion)
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        for (a, order, raw) in self.resolve_kind("service") {
            let Some(desc) = opt_text(&a, "service_description") else {
                self.err("service definition without service_description".into(), Some((&raw.file, raw.line)));
                continue;
            };
            let hosts = self.expand_hosts(&a, &cfg, &hostgroups, &raw);
            if hosts.is_empty() {
                self.warn(format!("service '{desc}' is not associated with any host"), Some((&raw.file, raw.line)));
                continue;
            }
            let mut common = self.checkable(&a, &order, &raw, false);
            if common.display_name.is_empty() {
                common.display_name = desc.clone();
            }
            for host in hosts {
                let mut c = common.clone();
                let h = &cfg.hosts[host_idx[&host]];
                // implicit inheritance from host (Nagios 3)
                if c.contacts.is_empty() && c.contact_groups.is_empty() {
                    c.contacts = h.common.contacts.clone();
                    c.contact_groups = h.common.contact_groups.clone();
                }
                if !a.contains_key("notification_interval") {
                    c.notification_interval = h.common.notification_interval;
                }
                if c.notification_period.is_none() {
                    c.notification_period = h.common.notification_period.clone();
                }
                let svc = ServiceConfig {
                    host_name: host.clone(),
                    description: desc.clone(),
                    is_volatile: flag(&a, "is_volatile", false),
                    servicegroups: a.get("servicegroups").map(|v| split_list(v)).unwrap_or_default(),
                    common: c,
                };
                let key = (host, desc.clone());
                if let Some(&prev) = seen.get(&key) {
                    self.warn(
                        format!("duplicate definition of service '{}' on host '{}', using the later one", key.1, key.0),
                        Some((&raw.file, raw.line)),
                    );
                    cfg.services[prev] = svc;
                } else {
                    seen.insert(key, cfg.services.len());
                    cfg.services.push(svc);
                }
            }
        }

        // service group membership from both sides
        let sg_index: HashMap<String, usize> =
            servicegroups.iter().enumerate().map(|(i, g)| (g.name.clone(), i)).collect();
        for svc in &cfg.services {
            for g in &svc.servicegroups {
                match sg_index.get(g) {
                    Some(&i) => {
                        let m = (svc.host_name.clone(), svc.description.clone());
                        if !servicegroups[i].members.contains(&m) {
                            servicegroups[i].members.push(m);
                        }
                    }
                    None => self.diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "service '{}' on '{}' refers to unknown servicegroup '{g}'",
                            svc.description, svc.host_name
                        ),
                        location: Some((svc.common.file.clone(), svc.common.line)),
                    }),
                }
            }
        }
        for g in &mut servicegroups {
            g.members.retain(|m| seen.contains_key(m));
            for m in &g.members {
                let s = &mut cfg.services[seen[m]];
                if !s.servicegroups.contains(&g.name) {
                    s.servicegroups.push(g.name.clone());
                }
            }
        }

        // contact group membership from both sides
        let cg_index: HashMap<String, usize> =
            contactgroups.iter().enumerate().map(|(i, g)| (g.name.clone(), i)).collect();
        for c in &cfg.contacts {
            for g in &c.contactgroups {
                if let Some(&i) = cg_index.get(g) {
                    if !contactgroups[i].members.contains(&c.name) {
                        contactgroups[i].members.push(c.name.clone());
                    }
                }
            }
        }
        let contact_idx: HashMap<String, usize> =
            cfg.contacts.iter().enumerate().map(|(i, c)| (c.name.clone(), i)).collect();
        for g in &contactgroups {
            for m in &g.members {
                match contact_idx.get(m) {
                    Some(&i) => {
                        if !cfg.contacts[i].contactgroups.contains(&g.name) {
                            cfg.contacts[i].contactgroups.push(g.name.clone());
                        }
                    }
                    None => self.diags.push(Diagnostic {
                        severity: Severity::Error,
                        message: format!("contactgroup '{}' refers to unknown contact '{m}'", g.name),
                        location: None,
                    }),
                }
            }
        }

        // --- service dependencies
        for (a, _, raw) in self.resolve_kind("servicedependency") {
            let masters = self.dependency_hosts(&a, "host_name", "hostgroup_name", &hostgroups, &raw);
            let same_host = !a.contains_key("dependent_host_name") && !a.contains_key("dependent_hostgroup_name");
            let dependents = if same_host {
                Vec::new()
            } else {
                self.dependency_hosts(&a, "dependent_host_name", "dependent_hostgroup_name", &hostgroups, &raw)
            };
            let services = split_list(&text(&a, "service_description"));
            let dependent_services = split_list(&text(&a, "dependent_service_description"));
            for master in &masters {
                // without dependent hosts every host depends on its own services
                let dependents = if same_host { std::slice::from_ref(master) } else { &dependents[..] };
                for dependent in dependents {
                    for service in &services {
                        for dependent_service in &dependent_services {
                            cfg.service_dependencies.push(ServiceDependencyConfig {
                                host_name: master.clone(),
                                service_description: service.clone(),
                                dependent_host_name: dependent.clone(),
                                dependent_service_description: dependent_service.clone(),
                                notification_failure_criteria: text(&a, "notification_failure_criteria"),
                                execution_failure_criteria: text(&a, "execution_failure_criteria"),
                                inherits_parent: flag(&a, "inherits_parent", false),
                                dependency_period: opt_text(&a, "dependency_period"),
                            });
                        }
                    }
                }
            }
        }

        // --- host dependencies
        for (a, _, raw) in self.resolve_kind("hostdependency") {
            let masters = self.dependency_hosts(&a, "host_name", "hostgroup_name", &hostgroups, &raw);
            let dependents =
                self.dependency_hosts(&a, "dependent_host_name", "dependent_hostgroup_name", &hostgroups, &raw);
            for master in &masters {
                for dependent in &dependents {
                    cfg.host_dependencies.push(HostDependencyConfig {
                        host_name: master.clone(),
                        dependent_host_name: dependent.clone(),
                        notification_failure_criteria: text(&a, "notification_failure_criteria"),
                        execution_failure_criteria: text(&a, "execution_failure_criteria"),
                        inherits_parent: flag(&a, "inherits_parent", false),
                        dependency_period: opt_text(&a, "dependency_period"),
                    });
                }
            }
        }

        // --- escalations
        for (kind, is_host) in [("hostescalation", true), ("serviceescalation", false)] {
            for (a, _, raw) in self.resolve_kind(kind) {
                let hosts = self.dependency_hosts(&a, "host_name", "hostgroup_name", &hostgroups, &raw);
                let services: Vec<Option<String>> = if is_host {
                    vec![None]
                } else {
                    split_list(&text(&a, "service_description")).into_iter().map(Some).collect()
                };
                let all = if is_host { StateOptions::ALL_HOST } else { StateOptions::ALL_SERVICE };
                for host in &hosts {
                    for service in &services {
                        cfg.escalations.push(EscalationConfig {
                            host_name: host.clone(),
                            service_description: service.clone(),
                            contacts: a.get("contacts").map(|v| split_list(v)).unwrap_or_default(),
                            contact_groups: a.get("contact_groups").map(|v| split_list(v)).unwrap_or_default(),
                            first_notification: num(&a, "first_notification", 0.0) as u32,
                            last_notification: num(&a, "last_notification", 0.0) as u32,
                            notification_interval: a.get("notification_interval").and_then(|v| v.trim().parse().ok()),
                            escalation_period: opt_text(&a, "escalation_period"),
                            escalation_options: a
                                .get("escalation_options")
                                .map_or(all, |v| StateOptions::parse(v, is_host)),
                        });
                    }
                }
            }
        }

        // Remaining kinds: accepted but not evaluated.
        let leftover: Vec<String> = self.by_kind.keys().cloned().collect();
        for kind in leftover {
            let known = matches!(kind.as_str(), "hostextinfo" | "serviceextinfo");
            let n = self.by_kind.remove(&kind).map(|v| v.len()).unwrap_or(0);
            if known {
                self.warn(format!("{n} {kind} definition(s) are accepted but not evaluated by corax"), None);
            } else {
                self.err(format!("unknown object type '{kind}'"), None);
            }
        }

        cfg.hostgroups = hostgroups;
        cfg.servicegroups = servicegroups;
        cfg.contactgroups = contactgroups;
        self.validate(&cfg);
        cfg
    }

    /// The hosts named by a host list and a hostgroup list of a dependency.
    fn dependency_hosts(
        &mut self,
        a: &Attrs,
        hosts_key: &str,
        groups_key: &str,
        groups: &[GroupConfig],
        raw: &RawObject,
    ) -> Vec<String> {
        let mut hosts = a.get(hosts_key).map(|v| split_list(v)).unwrap_or_default();
        for name in a.get(groups_key).map(|v| split_list(v)).unwrap_or_default() {
            match groups.iter().find(|g| g.name == name) {
                Some(g) => hosts.extend(g.members.iter().cloned()),
                None => {
                    self.err(format!("dependency refers to unknown hostgroup '{name}'"), Some((&raw.file, raw.line)))
                }
            }
        }
        let mut seen = HashSet::new();
        hosts.retain(|h| seen.insert(h.clone()));
        hosts
    }

    /// Expand `host_name` / `hostgroup_name` lists incl. `*` and `!exclusions`.
    fn expand_hosts(&mut self, a: &Attrs, cfg: &ObjectConfig, groups: &[GroupConfig], raw: &RawObject) -> Vec<String> {
        let mut include: Vec<String> = Vec::new();
        let mut exclude: HashSet<String> = HashSet::new();
        for item in a.get("host_name").map(|v| split_list(v)).unwrap_or_default() {
            if item == "*" {
                include.extend(cfg.hosts.iter().map(|h| h.name.clone()));
            } else if let Some(ex) = item.strip_prefix('!') {
                exclude.insert(ex.to_string());
            } else if cfg.hosts.iter().any(|h| h.name == item) {
                include.push(item);
            } else {
                self.err(format!("service refers to unknown host '{item}'"), Some((&raw.file, raw.line)));
            }
        }
        for item in a.get("hostgroup_name").map(|v| split_list(v)).unwrap_or_default() {
            let (neg, name) = match item.strip_prefix('!') {
                Some(n) => (true, n.to_string()),
                None => (false, item.clone()),
            };
            let members: Vec<String> = if name == "*" {
                cfg.hosts.iter().map(|h| h.name.clone()).collect()
            } else {
                match groups.iter().find(|g| g.name == name) {
                    Some(g) => g.members.clone(),
                    None => {
                        self.err(format!("service refers to unknown hostgroup '{name}'"), Some((&raw.file, raw.line)));
                        continue;
                    }
                }
            };
            if neg {
                exclude.extend(members);
            } else {
                include.extend(members);
            }
        }
        let mut seen = HashSet::new();
        include.retain(|h| !exclude.contains(h) && seen.insert(h.clone()));
        include
    }

    fn validate(&mut self, cfg: &ObjectConfig) {
        let mut errors: Vec<(String, PathBuf, usize)> = Vec::new();
        let check_cmd = |c: &Option<CommandCall>,
                         what: &str,
                         errors: &mut Vec<(String, PathBuf, usize)>,
                         file: &Path,
                         line: usize| {
            if let Some(c) = c {
                if !cfg.commands.contains_key(&c.name) {
                    errors.push((format!("{what}: command '{}' is not defined", c.name), file.to_path_buf(), line));
                }
            }
        };
        let check_tp =
            |tp: &Option<String>, what: &str, errors: &mut Vec<(String, PathBuf, usize)>, file: &Path, line: usize| {
                if let Some(tp) = tp {
                    if !cfg.timeperiods.contains_key(tp) {
                        errors.push((format!("{what}: timeperiod '{tp}' is not defined"), file.to_path_buf(), line));
                    }
                }
            };
        let contact_names: HashSet<&str> = cfg.contacts.iter().map(|c| c.name.as_str()).collect();
        let cg_names: HashSet<&str> = cfg.contactgroups.iter().map(|c| c.name.as_str()).collect();
        let host_names: HashSet<&str> = cfg.hosts.iter().map(|h| h.name.as_str()).collect();

        let check_common = |c: &CheckableConfig, what: String, errors: &mut Vec<(String, PathBuf, usize)>| {
            check_cmd(&c.check_command, &what, errors, &c.file, c.line);
            check_cmd(&c.event_handler, &what, errors, &c.file, c.line);
            check_tp(&c.check_period, &what, errors, &c.file, c.line);
            check_tp(&c.notification_period, &what, errors, &c.file, c.line);
            for ct in &c.contacts {
                if !contact_names.contains(ct.as_str()) {
                    errors.push((format!("{what}: contact '{ct}' is not defined"), c.file.clone(), c.line));
                }
            }
            for g in &c.contact_groups {
                if !cg_names.contains(g.as_str()) {
                    errors.push((format!("{what}: contactgroup '{g}' is not defined"), c.file.clone(), c.line));
                }
            }
        };
        for h in &cfg.hosts {
            check_common(&h.common, format!("host '{}'", h.name), &mut errors);
            for p in &h.parents {
                if !host_names.contains(p.as_str()) {
                    errors.push((
                        format!("host '{}': parent host '{p}' is not defined", h.name),
                        h.common.file.clone(),
                        h.common.line,
                    ));
                }
            }
        }
        for s in &cfg.services {
            if s.common.check_command.is_none() {
                errors.push((
                    format!("service '{}' on host '{}' has no check_command", s.description, s.host_name),
                    s.common.file.clone(),
                    s.common.line,
                ));
            }
            check_common(&s.common, format!("service '{}' on host '{}'", s.description, s.host_name), &mut errors);
        }
        for c in &cfg.contacts {
            for cmd in c.host_notification_commands.iter().chain(&c.service_notification_commands) {
                if !cfg.commands.contains_key(&cmd.name) {
                    errors.push((
                        format!("contact '{}': notification command '{}' is not defined", c.name, cmd.name),
                        PathBuf::new(),
                        0,
                    ));
                }
            }
            check_tp(&c.host_notification_period, &format!("contact '{}'", c.name), &mut errors, Path::new(""), 0);
            check_tp(&c.service_notification_period, &format!("contact '{}'", c.name), &mut errors, Path::new(""), 0);
        }
        for (msg, file, line) in errors {
            let loc = (line > 0).then_some((file.as_path(), line));
            self.err(msg, loc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(text: &str) -> Result<(ObjectConfig, Vec<Diagnostic>), ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("objects.cfg");
        std::fs::write(&path, text).unwrap();
        load_objects(&[path])
    }

    const BASE: &str = r#"
define command {
  command_name check-ping
  command_line /bin/true $HOSTADDRESS$
}
define command {
  command_name dummy
  command_line echo ok
}
define timeperiod {
  timeperiod_name 24X7
  monday 00:00-24:00
}
define host {
  name tmpl
  register 0
  max_check_attempts 3
  check_command check-ping
  check_period 24X7
  hostgroups +base
  _SITE corax
}
define hostgroup {
  hostgroup_name base
}
define hostgroup {
  hostgroup_name extra
}
"#;

    #[test]
    fn templates_and_additive_inheritance() {
        let (cfg, _) = resolve(&format!(
            "{BASE}\ndefine host {{\n use tmpl\n host_name h1\n address 10.0.0.1\n hostgroups +extra\n _TAGS a b\n}}\n\
             define service {{\n host_name h1\n service_description CPU\n check_command dummy\n max_check_attempts 1\n servicegroups sg\n}}\n\
             define servicegroup {{\n servicegroup_name sg\n}}\n"
        ))
        .unwrap();
        let h = &cfg.hosts[0];
        assert_eq!(h.common.max_check_attempts, 3);
        assert_eq!(h.hostgroups, vec!["base".to_string(), "extra".to_string()]);
        assert_eq!(h.common.custom_var("TAGS"), Some("a b"));
        assert_eq!(h.common.custom_var("SITE"), Some("corax"));
        assert_eq!(cfg.hostgroups.iter().find(|g| g.name == "base").unwrap().members, vec!["h1"]);
        assert_eq!(cfg.servicegroups[0].members, vec![("h1".to_string(), "CPU".to_string())]);
        assert_eq!(cfg.services[0].servicegroups, vec!["sg"]);
    }

    #[test]
    fn hostgroup_expansion_and_exclusion() {
        let (cfg, _) = resolve(&format!(
            "{BASE}\ndefine host {{\n use tmpl\n host_name a\n}}\ndefine host {{\n use tmpl\n host_name b\n}}\n\
             define service {{\n hostgroup_name base\n host_name !b\n service_description X\n check_command dummy\n max_check_attempts 1\n}}\n"
        ))
        .unwrap();
        assert_eq!(cfg.services.len(), 1);
        assert_eq!(cfg.services[0].host_name, "a");
    }

    #[test]
    fn unknown_command_is_error() {
        let err = resolve(&format!("{BASE}\ndefine host {{\n use tmpl\n host_name a\n check_command nope!1\n}}\n"))
            .unwrap_err();
        assert!(err.to_string().contains("command 'nope' is not defined"), "{err}");
    }

    #[test]
    fn command_args() {
        let c = CommandCall::parse("check-mk-host-ping!-w 200.00,80.00% -c 500.00,100.00%");
        assert_eq!(c.name, "check-mk-host-ping");
        assert_eq!(c.args, vec!["-w 200.00,80.00% -c 500.00,100.00%"]);
    }

    #[test]
    fn dependencies_expand_lists_and_hostgroups() {
        let base = "define command {\n command_name dummy\n command_line /bin/true\n}\n\
            define host {\n host_name a\n address 1\n max_check_attempts 1\n hostgroups web\n}\n\
            define host {\n host_name b\n address 2\n max_check_attempts 1\n hostgroups web\n}\n\
            define hostgroup {\n hostgroup_name web\n}\n\
            define service {\n hostgroup_name web\n service_description Agent\n check_command dummy\n max_check_attempts 1\n}\n\
            define service {\n hostgroup_name web\n service_description X\n check_command dummy\n max_check_attempts 1\n}\n\
            define service {\n hostgroup_name web\n service_description Y\n check_command dummy\n max_check_attempts 1\n}\n";
        let deps = "define servicedependency {\n hostgroup_name web\n service_description Agent\n \
            dependent_service_description X,Y\n notification_failure_criteria c\n inherits_parent 1\n}\n\
            define hostdependency {\n host_name a\n dependent_hostgroup_name web\n notification_failure_criteria d\n}\n";
        let (cfg, diags) = resolve(&format!("{base}{deps}")).unwrap();
        assert!(diags.iter().all(|d| d.severity != Severity::Error), "{diags:?}");
        // every host of the group depends on its own service
        let pairs: Vec<String> = cfg
            .service_dependencies
            .iter()
            .map(|d| {
                format!(
                    "{}/{}>{}/{}",
                    d.dependent_host_name, d.dependent_service_description, d.host_name, d.service_description
                )
            })
            .collect();
        assert_eq!(pairs, ["a/X>a/Agent", "a/Y>a/Agent", "b/X>b/Agent", "b/Y>b/Agent"]);
        assert!(cfg.service_dependencies[0].inherits_parent);
        let hosts: Vec<(&str, &str)> =
            cfg.host_dependencies.iter().map(|d| (d.host_name.as_str(), d.dependent_host_name.as_str())).collect();
        assert_eq!(hosts, [("a", "a"), ("a", "b")]);
    }
}
