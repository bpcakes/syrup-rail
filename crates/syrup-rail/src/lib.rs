//! Application-independent subscription billing domain types and policy.
//!
//! PostgreSQL orchestration lives in `syrup-rail-postgres`; NMI adaptation lives
//! in `syrup-rail-nmi` and `syrup-rail-nmi-client`.

#![forbid(unsafe_code)]

mod card_data;
mod event;
mod gateway;
mod gateway_value;
mod identity;
mod money;
mod policy;
mod resolution;
mod subscription;

pub use card_data::{raw_card_data_ranges, string_contains_raw_card_data};
pub use event::*;
pub use gateway::*;
pub use gateway_value::*;
pub use identity::*;
pub use money::*;
pub use policy::*;
pub use resolution::*;
pub use subscription::*;

/// Workspace package version exposed for consumer pinning diagnostics.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
