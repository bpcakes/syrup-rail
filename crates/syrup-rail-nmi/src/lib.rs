//! NMI gateway and lifecycle-evidence adapter for Syrup Rail.

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod adapter;
mod decline_advice;
mod lifecycle;
mod reference;

pub use adapter::NmiPaymentGateway;
pub use decline_advice::{NMI_RETRY_IN_A_FEW_DAYS_DELAY, nmi_renewal_decline_retry_advice};
pub use reference::{NmiMutationNamespace, NmiMutationReferenceFactory, NmiNamespaceError};
pub use syrup_rail_nmi_client as nmi_client;
