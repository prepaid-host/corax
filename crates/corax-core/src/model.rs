//! Runtime object model: hosts, services, contacts, comments, downtimes.
//!
//! Hosts and services share [`CheckState`]. State codes follow Nagios conventions:
//! services `0=OK 1=WARNING 2=CRITICAL 3=UNKNOWN`, hosts `0=UP 1=DOWN 2=UNREACHABLE`.

use corax_config::{ContactConfig, HostConfig, ServiceConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContactId(pub u32);

/// Either a host or a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    Host(HostId),
    Service(ServiceId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum StateType {
    Soft = 0,
    #[default]
    Hard = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CheckType {
    #[default]
    Active = 0,
    Passive = 1,
}

/// Bits for `modified_attributes` (Nagios `MODATTR_*`).
pub mod modattr {
    pub const NOTIFICATIONS_ENABLED: u32 = 1;
    pub const ACTIVE_CHECKS_ENABLED: u32 = 2;
    pub const PASSIVE_CHECKS_ENABLED: u32 = 4;
    pub const EVENT_HANDLER_ENABLED: u32 = 8;
    pub const FLAP_DETECTION_ENABLED: u32 = 16;
    pub const FAILURE_PREDICTION_ENABLED: u32 = 32;
    pub const PERFORMANCE_DATA_ENABLED: u32 = 64;
    pub const OBSESSIVE_HANDLER_ENABLED: u32 = 128;
    pub const EVENT_HANDLER_COMMAND: u32 = 256;
    pub const CHECK_COMMAND: u32 = 512;
    pub const NORMAL_CHECK_INTERVAL: u32 = 1024;
    pub const RETRY_CHECK_INTERVAL: u32 = 2048;
    pub const MAX_CHECK_ATTEMPTS: u32 = 4096;
    pub const FRESHNESS_CHECKS_ENABLED: u32 = 8192;
    pub const CHECK_TIMEPERIOD: u32 = 16384;
    pub const CUSTOM_VARIABLE: u32 = 32768;
    pub const NOTIFICATION_TIMEPERIOD: u32 = 65536;

    /// Names as used by Livestatus `modified_attributes_list`.
    pub const NAMES: [(u32, &str); 17] = [
        (NOTIFICATIONS_ENABLED, "notifications_enabled"),
        (ACTIVE_CHECKS_ENABLED, "active_checks_enabled"),
        (PASSIVE_CHECKS_ENABLED, "passive_checks_enabled"),
        (EVENT_HANDLER_ENABLED, "event_handler_enabled"),
        (FLAP_DETECTION_ENABLED, "flap_detection_enabled"),
        (FAILURE_PREDICTION_ENABLED, "failure_prediction_enabled"),
        (PERFORMANCE_DATA_ENABLED, "performance_data_enabled"),
        (OBSESSIVE_HANDLER_ENABLED, "obsessive_handler_enabled"),
        (EVENT_HANDLER_COMMAND, "event_handler_command"),
        (CHECK_COMMAND, "check_command"),
        (NORMAL_CHECK_INTERVAL, "normal_check_interval"),
        (RETRY_CHECK_INTERVAL, "retry_check_interval"),
        (MAX_CHECK_ATTEMPTS, "max_check_attempts"),
        (FRESHNESS_CHECKS_ENABLED, "freshness_checks_enabled"),
        (CHECK_TIMEPERIOD, "check_timeperiod"),
        (CUSTOM_VARIABLE, "custom_variable"),
        (NOTIFICATION_TIMEPERIOD, "notification_timeperiod"),
    ];
}

/// Number of state history slots used for flap detection (Nagios: 21).
pub const FLAP_HISTORY: usize = 21;

/// Mutable check state of a host or service.
#[derive(Debug, Clone)]
pub struct CheckState {
    pub current_state: u8,
    pub last_state: u8,
    pub last_hard_state: u8,
    pub state_type: StateType,
    pub current_attempt: u32,
    pub has_been_checked: bool,
    pub check_type: CheckType,

    pub plugin_output: String,
    pub long_plugin_output: String,
    pub perf_data: String,

    pub last_check: i64,
    pub next_check: i64,
    pub last_state_change: i64,
    pub last_hard_state_change: i64,
    /// Indexed by state code (0..=3): last time the object was in that state.
    pub last_time_in_state: [i64; 4],
    pub latency: f64,
    pub execution_time: f64,
    pub is_executing: bool,
    /// The next scheduled check was forced (runs even if active checks are disabled).
    pub forced_check: bool,
    /// Bumped whenever the object is (re)scheduled; stale queue entries are ignored.
    pub sched_generation: u32,

    pub acknowledged: bool,
    /// 0 = none, 1 = normal, 2 = sticky.
    pub acknowledgement_type: u8,
    /// Unix time at which the acknowledgement expires (0 = never).
    pub acknowledgement_expire_time: i64,

    pub notifications_enabled: bool,
    pub active_checks_enabled: bool,
    pub passive_checks_enabled: bool,
    pub event_handler_enabled: bool,
    pub flap_detection_enabled: bool,
    pub check_freshness: bool,
    pub process_performance_data: bool,
    pub obsess: bool,
    pub modified_attributes: u32,

    pub scheduled_downtime_depth: u32,
    pub is_flapping: bool,
    pub percent_state_change: f64,
    pub state_history: [u8; FLAP_HISTORY],
    pub state_history_index: usize,
    pub state_history_len: usize,

    pub current_notification_number: u32,
    pub last_notification: i64,
    pub next_notification: i64,
    pub no_more_notifications: bool,
    /// Why a due notification was not sent yet (Livestatus `notification_postponement_reason`).
    pub notification_postponement_reason: String,
    pub current_problem_id: u64,
    pub last_problem_id: u64,
    pub current_event_id: u64,
    pub last_event_id: u64,

    /// Checkmk "cached" checks: (cached_at, cache_interval) if output carried it.
    pub cached_at: i64,
    pub cache_interval: i64,
}

impl CheckState {
    pub fn new(cfg: &corax_config::CheckableConfig) -> Self {
        CheckState {
            current_state: 0,
            last_state: 0,
            last_hard_state: 0,
            state_type: StateType::Hard,
            current_attempt: 1,
            has_been_checked: false,
            check_type: CheckType::Active,
            plugin_output: String::new(),
            long_plugin_output: String::new(),
            perf_data: String::new(),
            last_check: 0,
            next_check: 0,
            last_state_change: 0,
            last_hard_state_change: 0,
            last_time_in_state: [0; 4],
            latency: 0.0,
            execution_time: 0.0,
            is_executing: false,
            forced_check: false,
            sched_generation: 0,
            acknowledged: false,
            acknowledgement_type: 0,
            acknowledgement_expire_time: 0,
            notifications_enabled: cfg.notifications_enabled,
            active_checks_enabled: cfg.active_checks_enabled,
            passive_checks_enabled: cfg.passive_checks_enabled,
            event_handler_enabled: cfg.event_handler_enabled,
            flap_detection_enabled: cfg.flap_detection_enabled,
            check_freshness: cfg.check_freshness,
            process_performance_data: cfg.process_perf_data,
            obsess: cfg.obsess,
            modified_attributes: 0,
            scheduled_downtime_depth: 0,
            is_flapping: false,
            percent_state_change: 0.0,
            state_history: [0; FLAP_HISTORY],
            state_history_index: 0,
            state_history_len: 0,
            current_notification_number: 0,
            last_notification: 0,
            next_notification: 0,
            no_more_notifications: false,
            notification_postponement_reason: String::new(),
            current_problem_id: 0,
            last_problem_id: 0,
            current_event_id: 0,
            last_event_id: 0,
            cached_at: 0,
            cache_interval: 0,
        }
    }

    pub fn is_hard(&self) -> bool {
        self.state_type == StateType::Hard
    }

    /// Nagios "hard state" as shown by Livestatus `hard_state`.
    pub fn hard_state(&self) -> u8 {
        if self.is_hard() {
            self.current_state
        } else {
            self.last_hard_state
        }
    }
}

#[derive(Debug, Clone)]
pub struct Host {
    pub id: HostId,
    pub cfg: HostConfig,
    pub services: Vec<ServiceId>,
    pub parents: Vec<HostId>,
    pub children: Vec<HostId>,
    /// Contacts from `contacts` and members of `contact_groups`, deduplicated.
    pub contacts: Vec<ContactId>,
    pub st: CheckState,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub id: ServiceId,
    pub host: HostId,
    pub cfg: ServiceConfig,
    pub contacts: Vec<ContactId>,
    pub st: CheckState,
}

#[derive(Debug, Clone)]
pub struct Contact {
    pub id: ContactId,
    pub cfg: ContactConfig,
    pub host_notifications_enabled: bool,
    pub service_notifications_enabled: bool,
    pub last_host_notification: i64,
    pub last_service_notification: i64,
    pub modified_attributes: u32,
}

/// Comment entry types (Livestatus `entry_type`).
pub mod comment_entry {
    pub const USER: u8 = 1;
    pub const DOWNTIME: u8 = 2;
    pub const FLAPPING: u8 = 3;
    pub const ACKNOWLEDGEMENT: u8 = 4;
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub target: Target,
    pub entry_type: u8,
    pub author: String,
    pub text: String,
    pub entry_time: i64,
    pub persistent: bool,
    /// 1 = user, 2 = scheduled downtime, 3 = flapping, 4 = acknowledgement (Nagios `source`/`entry_type`).
    pub source: u8,
    pub expires: bool,
    pub expire_time: i64,
}

#[derive(Debug, Clone)]
pub struct Downtime {
    pub id: u64,
    pub target: Target,
    pub author: String,
    pub comment: String,
    pub entry_time: i64,
    pub start_time: i64,
    pub end_time: i64,
    pub fixed: bool,
    /// Duration in seconds (for flexible downtimes).
    pub duration: i64,
    pub triggered_by: u64,
    /// The downtime is currently in effect.
    pub is_in_effect: bool,
    /// Actual begin for flexible downtimes once triggered.
    pub flex_downtime_start: i64,
    /// Id of the comment created for this downtime.
    pub comment_id: u64,
}

/// Global program state (Livestatus `status` table, retention `program` block).
#[derive(Debug, Clone)]
pub struct ProgramState {
    pub program_start: i64,
    pub pid: u32,
    pub enable_notifications: bool,
    pub execute_service_checks: bool,
    pub accept_passive_service_checks: bool,
    pub execute_host_checks: bool,
    pub accept_passive_host_checks: bool,
    pub enable_event_handlers: bool,
    pub enable_flap_detection: bool,
    pub process_performance_data: bool,
    pub check_service_freshness: bool,
    pub check_host_freshness: bool,
    pub obsess_over_services: bool,
    pub obsess_over_hosts: bool,
    pub last_command_check: i64,
    pub last_log_rotation: i64,
    pub next_comment_id: u64,
    pub next_downtime_id: u64,
    pub next_event_id: u64,
    pub next_problem_id: u64,
    pub next_notification_id: u64,
    pub modified_host_attributes: u32,
    pub modified_service_attributes: u32,
}

/// Runtime counters for the Livestatus `status` table (`*_rate` columns are derived).
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub service_checks: u64,
    pub host_checks: u64,
    pub external_commands: u64,
    pub log_messages: u64,
    pub livestatus_queries: u64,
    pub livestatus_connections: u64,
    pub neb_callbacks: u64,
    pub forks: u64,
    pub helper_checks: u64,
}
