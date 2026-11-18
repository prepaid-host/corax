//! Side effects produced while the world lock is held, executed afterwards.

use crate::model::Target;
use crate::scheduler::Scheduler;
use crate::World;

/// Something the engine must do after releasing the world lock.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Append a line to `nagios.log` (`time`, text without `[time] ` prefix).
    Log(i64, String),
    /// Run a notification command.
    Notify { cmdline: String },
    /// Run an event handler.
    EventHandler { cmdline: String },
    /// npcdmod perfdata line for a host or service.
    Perfdata(Target),
    /// Persist retention data now (e.g. `SAVE_STATE_INFORMATION`).
    SaveRetention,
    /// Reload the configuration (`RESTART_PROCESS`).
    Restart,
    /// Shut down (`SHUTDOWN_PROCESS`).
    Shutdown,
}

/// Mutable context passed through all state-changing functions.
pub struct Ctx<'a> {
    pub w: &'a mut World,
    pub sched: &'a mut Scheduler,
    pub fx: &'a mut Vec<Effect>,
    pub now: i64,
}

impl Ctx<'_> {
    pub fn log(&mut self, text: impl Into<String>) {
        self.fx.push(Effect::Log(self.now, text.into()));
    }

    pub fn log_at(&mut self, time: i64, text: impl Into<String>) {
        self.fx.push(Effect::Log(time, text.into()));
    }
}
