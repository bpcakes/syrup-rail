//! Application-independent subscription billing domain types and policy.
//!
//! PostgreSQL orchestration lives in `syrup-rail-postgres`; NMI adaptation lives
//! in `syrup-rail-nmi` and `syrup-rail-nmi-client`.

#![forbid(unsafe_code)]

mod admission;
mod attempt;
mod card_data;
mod discount;
mod enrollment;
mod event;
mod gateway;
mod gateway_value;
mod identity;
mod money;
mod payment_method_update;
mod policy;
mod recovery;
mod resolution;
mod resolver;
mod subscription;

pub use admission::*;
pub use attempt::*;
pub use card_data::{raw_card_data_ranges, string_contains_raw_card_data};
pub use discount::*;
pub use enrollment::*;
pub use event::*;
pub use gateway::*;
pub use gateway_value::*;
pub use identity::*;
pub use money::*;
pub use payment_method_update::*;
pub use policy::*;
pub use recovery::*;
pub use resolution::*;
pub use resolver::*;
pub use subscription::*;

/// Workspace package version exposed for consumer pinning diagnostics.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
