//! NMI gateway and lifecycle-evidence adapter for Syrup Rail.

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod adapter;
mod approval_evidence;
mod lifecycle;
mod reference;

pub use adapter::NmiPaymentGateway;
pub use reference::{NmiMutationNamespace, NmiMutationReferenceFactory, NmiNamespaceError};
pub use syrup_rail_nmi_client as nmi_client;
