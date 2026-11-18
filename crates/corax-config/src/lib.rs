//! Parser and resolver for Nagios-style configuration.
//!
//! This crate turns a Nagios 3 compatible main configuration file (`nagios.cfg`) and the
//! object definition files it references into a fully resolved, validated
//! [`ObjectConfig`]. Template inheritance (`use`, `name`, `register 0`), additive
//! inheritance (`+value`), custom variables, host/service/contact group membership and
//! service host lists are all resolved here, so that the runtime never needs to know
//! about templates.
//!
//! The crate is pure (no async, no global state) and is used both by `corax -v`
//! (configuration verification) and by the running daemon.

mod error;
mod lexer;
mod main_cfg;
mod resolve;
pub mod timeperiod;
mod types;

pub use error::{ConfigError, Diagnostic, Severity};
pub use lexer::{parse_object_text, RawObject};
pub use main_cfg::{BrokerModule, MainConfig, Resources};
pub use resolve::{load, load_objects, LoadedConfig};
pub use timeperiod::Timeperiod;
pub use types::*;
