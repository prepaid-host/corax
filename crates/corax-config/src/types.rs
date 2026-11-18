//! Fully resolved object types (templates already applied).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::timeperiod::Timeperiod;

/// A command invocation such as `check_mk-df!/var` split into name and arguments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandCall {
    pub name: String,
    pub args: Vec<String>,
    /// The original, unsplit text (shown by Livestatus `check_command`).
    pub raw: String,
}

impl CommandCall {
    pub fn parse(raw: &str) -> Self {
        let mut parts = split_bang(raw).into_iter();
        let name = parts.next().unwrap_or_default();
        Self { name, args: parts.collect(), raw: raw.to_string() }
    }
}

/// Split on `!`, honouring `\!` escapes.
fn split_bang(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'!') => {
                out.last_mut().expect("non-empty").push('!');
                chars.next();
            }
            '!' => out.push(String::new()),
            _ => out.last_mut().expect("non-empty").push(c),
        }
    }
    out
}

/// Notification / flap-detection / stalking option letters as a bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateOptions(pub u16);

impl StateOptions {
    pub const OK_UP: u16 = 1 << 0; // `o`
    pub const WARNING_DOWN: u16 = 1 << 1; // `w` (service) / `d` (host)
    pub const UNKNOWN_UNREACHABLE: u16 = 1 << 2; // `u`
    pub const CRITICAL: u16 = 1 << 3; // `c`
    pub const RECOVERY: u16 = 1 << 4; // `r`
    pub const FLAPPING: u16 = 1 << 5; // `f`
    pub const DOWNTIME: u16 = 1 << 6; // `s`

    pub const ALL_SERVICE: StateOptions = StateOptions(
        Self::WARNING_DOWN
            | Self::UNKNOWN_UNREACHABLE
            | Self::CRITICAL
            | Self::RECOVERY
            | Self::FLAPPING
            | Self::DOWNTIME,
    );
    pub const ALL_HOST: StateOptions =
        StateOptions(Self::WARNING_DOWN | Self::UNKNOWN_UNREACHABLE | Self::RECOVERY | Self::FLAPPING | Self::DOWNTIME);

    /// Parse e.g. `d,u,r,f,s` or `w,u,c,r`. `n` means none, `a` means all.
    pub fn parse(s: &str, is_host: bool) -> Self {
        let mut bits = 0;
        for opt in s.split(',').map(str::trim) {
            bits |= match opt {
                "o" => Self::OK_UP,
                "w" | "d" => Self::WARNING_DOWN,
                "u" => Self::UNKNOWN_UNREACHABLE,
                "c" => Self::CRITICAL,
                "r" => Self::RECOVERY,
                "f" => Self::FLAPPING,
                "s" => Self::DOWNTIME,
                "a" => return if is_host { Self::ALL_HOST } else { Self::ALL_SERVICE },
                _ => 0,
            };
        }
        StateOptions(bits)
    }

    pub fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }

    /// Whether a service (`is_host == false`) or host problem state is selected.
    pub fn has_state(self, state: u8, is_host: bool) -> bool {
        let bit = match (is_host, state) {
            (_, 0) => Self::RECOVERY,
            (true, 1) => Self::WARNING_DOWN,
            (true, _) => Self::UNKNOWN_UNREACHABLE,
            (false, 1) => Self::WARNING_DOWN,
            (false, 2) => Self::CRITICAL,
            (false, _) => Self::UNKNOWN_UNREACHABLE,
        };
        self.has(bit)
    }
}

/// Attributes shared by hosts and services.
#[derive(Debug, Clone)]
pub struct CheckableConfig {
    pub display_name: String,
    pub check_command: Option<CommandCall>,
    /// In units of `interval_length` (minutes by default). Fractions allowed.
    pub check_interval: f64,
    pub retry_interval: f64,
    pub max_check_attempts: u32,
    pub check_period: Option<String>,
    pub notification_period: Option<String>,
    pub notification_interval: f64,
    pub first_notification_delay: f64,
    pub notification_options: StateOptions,
    pub notifications_enabled: bool,
    pub active_checks_enabled: bool,
    pub passive_checks_enabled: bool,
    pub check_freshness: bool,
    pub freshness_threshold: i64,
    pub event_handler: Option<CommandCall>,
    pub event_handler_enabled: bool,
    pub flap_detection_enabled: bool,
    pub low_flap_threshold: f64,
    pub high_flap_threshold: f64,
    pub process_perf_data: bool,
    pub obsess: bool,
    pub contacts: Vec<String>,
    pub contact_groups: Vec<String>,
    pub notes: String,
    pub notes_url: String,
    pub action_url: String,
    pub icon_image: String,
    pub icon_image_alt: String,
    /// Custom variables without the leading underscore, upper-cased, in file order.
    pub custom_vars: Vec<(String, String)>,
    pub file: PathBuf,
    pub line: usize,
}

impl CheckableConfig {
    pub fn custom_var(&self, name: &str) -> Option<&str> {
        self.custom_vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub alias: String,
    pub address: String,
    pub parents: Vec<String>,
    /// Host groups this host belongs to (resolved from both sides).
    pub hostgroups: Vec<String>,
    pub statusmap_image: String,
    pub common: CheckableConfig,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub host_name: String,
    pub description: String,
    pub is_volatile: bool,
    /// Service groups this service belongs to (resolved from both sides).
    pub servicegroups: Vec<String>,
    pub common: CheckableConfig,
}

#[derive(Debug, Clone)]
pub struct CommandConfig {
    pub name: String,
    pub line: String,
}

#[derive(Debug, Clone)]
pub struct ContactConfig {
    pub name: String,
    pub alias: String,
    pub email: String,
    pub pager: String,
    pub addresses: Vec<String>,
    pub host_notification_period: Option<String>,
    pub service_notification_period: Option<String>,
    pub host_notification_options: StateOptions,
    pub service_notification_options: StateOptions,
    pub host_notification_commands: Vec<CommandCall>,
    pub service_notification_commands: Vec<CommandCall>,
    pub host_notifications_enabled: bool,
    pub service_notifications_enabled: bool,
    pub can_submit_commands: bool,
    pub contactgroups: Vec<String>,
    pub custom_vars: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct GroupConfig {
    pub name: String,
    pub alias: String,
    pub notes: String,
    pub notes_url: String,
    pub action_url: String,
    /// Host or contact names.
    pub members: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceGroupConfig {
    pub name: String,
    pub alias: String,
    pub notes: String,
    pub notes_url: String,
    pub action_url: String,
    /// `(host_name, service_description)` pairs.
    pub members: Vec<(String, String)>,
}

/// A service dependency (parsed; evaluated for notification suppression only).
#[derive(Debug, Clone)]
pub struct ServiceDependencyConfig {
    pub host_name: String,
    pub service_description: String,
    pub dependent_host_name: String,
    pub dependent_service_description: String,
    pub notification_failure_criteria: String,
    pub execution_failure_criteria: String,
}

/// The fully resolved object configuration.
#[derive(Debug, Clone, Default)]
pub struct ObjectConfig {
    pub commands: HashMap<String, CommandConfig>,
    pub timeperiods: HashMap<String, Timeperiod>,
    pub contacts: Vec<ContactConfig>,
    pub contactgroups: Vec<GroupConfig>,
    pub hosts: Vec<HostConfig>,
    pub hostgroups: Vec<GroupConfig>,
    pub services: Vec<ServiceConfig>,
    pub servicegroups: Vec<ServiceGroupConfig>,
    pub service_dependencies: Vec<ServiceDependencyConfig>,
}
