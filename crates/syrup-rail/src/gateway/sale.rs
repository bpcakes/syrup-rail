use std::fmt;

use crate::{
    BillingContact, CardholderIpAddress, ChargeAmount, GatewayOrderId,
    GatewayPaymentMethodReference, GatewayTransactionId, PaymentToken, StatementDescriptor,
};

#[derive(Clone)]
pub enum GatewaySaleIntent {
    OneTime {
        payment_token: PaymentToken,
    },
    InitialStoredCredential {
        payment_token: PaymentToken,
    },
    RecurringStoredCredential {
        payment_method_reference: GatewayPaymentMethodReference,
        initial_transaction_id: GatewayTransactionId,
        /// True while the subscription is still in its paid-trial phase, so
        /// this merchant-initiated sale is the first recurring charge after
        /// the trial, including any dunning retry of that same charge. A
        /// [`StatementDescriptorPolicy`](crate::StatementDescriptorPolicy)
        /// uses it to select trial wording for the customer's statement.
        first_charge_after_paid_trial: bool,
    },
}

impl fmt::Debug for GatewaySaleIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OneTime { payment_token } => formatter
                .debug_struct("OneTime")
                .field("payment_token", payment_token)
                .finish(),
            Self::InitialStoredCredential { payment_token } => formatter
                .debug_struct("InitialStoredCredential")
                .field("payment_token", payment_token)
                .finish(),
            Self::RecurringStoredCredential {
                payment_method_reference,
                initial_transaction_id,
                first_charge_after_paid_trial,
            } => formatter
                .debug_struct("RecurringStoredCredential")
                .field("payment_method_reference", payment_method_reference)
                .field("initial_transaction_id", initial_transaction_id)
                .field(
                    "first_charge_after_paid_trial",
                    first_charge_after_paid_trial,
                )
                .finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GatewaySaleRequest {
    charge: ChargeAmount,
    order_id: GatewayOrderId,
    intent: GatewaySaleIntent,
    billing_contact: Option<BillingContact>,
    descriptor: Option<StatementDescriptor>,
    cardholder_ip: Option<CardholderIpAddress>,
}

/// The owned fields of a [`GatewaySaleRequest`], for decorators that rebuild
/// the request they forward.
#[derive(Clone, Debug)]
pub struct GatewaySaleRequestParts {
    pub charge: ChargeAmount,
    pub order_id: GatewayOrderId,
    pub intent: GatewaySaleIntent,
    pub billing_contact: Option<BillingContact>,
    /// The statement descriptor, if the request carries one. A decorator that
    /// rebuilds the request must pass it on with `with_descriptor`, or the
    /// provider prints the account's static descriptor.
    pub descriptor: Option<StatementDescriptor>,
    /// The cardholder's network address, if the request carries one. A
    /// decorator that rebuilds the request must pass it on with
    /// `with_cardholder_ip`, or the provider receives the sale without it.
    pub cardholder_ip: Option<CardholderIpAddress>,
}

impl GatewaySaleRequest {
    pub const fn new(
        charge: ChargeAmount,
        order_id: GatewayOrderId,
        intent: GatewaySaleIntent,
        billing_contact: Option<BillingContact>,
    ) -> Self {
        Self {
            charge,
            order_id,
            intent,
            billing_contact,
            descriptor: None,
            cardholder_ip: None,
        }
    }

    /// Asks the provider to print this descriptor for the sale, replacing any
    /// descriptor already set. Providers whose acquirer does not support
    /// dynamic descriptors ignore it.
    pub fn with_descriptor(mut self, descriptor: Option<StatementDescriptor>) -> Self {
        self.descriptor = descriptor;
        self
    }

    /// Tells the provider which network address the cardholder used for this
    /// sale, replacing any address already set. Only a sale the cardholder
    /// personally initiates may carry one; the subscription workflows set it
    /// on token sales and never on merchant-initiated stored-credential
    /// charges.
    pub const fn with_cardholder_ip(mut self, cardholder_ip: Option<CardholderIpAddress>) -> Self {
        self.cardholder_ip = cardholder_ip;
        self
    }

    pub const fn charge(&self) -> ChargeAmount {
        self.charge
    }

    pub const fn order_id(&self) -> &GatewayOrderId {
        &self.order_id
    }

    pub const fn intent(&self) -> &GatewaySaleIntent {
        &self.intent
    }

    pub const fn billing_contact(&self) -> Option<&BillingContact> {
        self.billing_contact.as_ref()
    }

    pub const fn descriptor(&self) -> Option<&StatementDescriptor> {
        self.descriptor.as_ref()
    }

    pub const fn cardholder_ip(&self) -> Option<CardholderIpAddress> {
        self.cardholder_ip
    }

    pub fn into_parts(self) -> GatewaySaleRequestParts {
        GatewaySaleRequestParts {
            charge: self.charge,
            order_id: self.order_id,
            intent: self.intent,
            billing_contact: self.billing_contact,
            descriptor: self.descriptor,
            cardholder_ip: self.cardholder_ip,
        }
    }
}

impl From<GatewaySaleRequestParts> for GatewaySaleRequest {
    fn from(parts: GatewaySaleRequestParts) -> Self {
        Self {
            charge: parts.charge,
            order_id: parts.order_id,
            intent: parts.intent,
            billing_contact: parts.billing_contact,
            descriptor: parts.descriptor,
            cardholder_ip: parts.cardholder_ip,
        }
    }
}
