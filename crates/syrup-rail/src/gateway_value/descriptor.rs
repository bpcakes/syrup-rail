use std::fmt;

use thiserror::Error;

use crate::GatewaySaleIntent;

/// Longest dynamic merchant-name text a host may ask a provider to print.
///
/// Card networks carry the merchant name in a 25-character field (Visa) or a
/// 22-character field (Mastercard), and an acquirer that enables dynamic
/// descriptors usually fixes a prefix inside that field. Hosts must budget
/// for that prefix from their acquirer's documentation; this limit only bounds
/// the text Syrup Rail will carry.
pub const MAX_STATEMENT_DESCRIPTOR_NAME_BYTES: usize = 25;
/// Longest customer-service phone text carried with a statement descriptor.
pub const MAX_STATEMENT_DESCRIPTOR_PHONE_BYTES: usize = 20;
/// Longest merchant URL text carried with a statement descriptor.
pub const MAX_STATEMENT_DESCRIPTOR_URL_BYTES: usize = 40;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum StatementDescriptorError {
    #[error("statement descriptor name is required")]
    MissingName,
    #[error("statement descriptor name exceeds {MAX_STATEMENT_DESCRIPTOR_NAME_BYTES} bytes")]
    NameTooLong,
    #[error("statement descriptor phone exceeds {MAX_STATEMENT_DESCRIPTOR_PHONE_BYTES} bytes")]
    PhoneTooLong,
    #[error("statement descriptor URL exceeds {MAX_STATEMENT_DESCRIPTOR_URL_BYTES} bytes")]
    UrlTooLong,
    #[error("statement descriptor field contains a character outside printable ASCII")]
    UnsupportedCharacter,
    #[error(
        "statement descriptor phone contains a character other than digits, spaces, +, -, (, ) or ."
    )]
    InvalidPhone,
    #[error("statement descriptor URL contains whitespace")]
    InvalidUrl,
}

/// Merchant-chosen text a provider may print on the cardholder's statement for
/// one sale, where the acquirer supports dynamic (soft) descriptors.
///
/// The name is required; the phone and URL are optional. Every field is
/// trimmed, limited to printable ASCII, and bounded by the `MAX_STATEMENT_
/// DESCRIPTOR_*_BYTES` constants. The phone accepts digits, spaces, `+`, `-`,
/// `(`, `)` and `.`; the URL accepts no whitespace. These are transport
/// bounds, not network rules: the networks print the phone or URL in a
/// 13-character field, and an acquirer usually prefixes the name. Hosts take
/// the exact strings from their acquirer. Local validation never proves that
/// the provider, acquirer or processor will honor the descriptor; where they
/// do not, the account's static descriptor prints instead.
///
/// A descriptor is merchant configuration meant for the customer's statement,
/// so ordinary `Debug` formatting shows its values.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StatementDescriptor {
    name: String,
    phone: Option<String>,
    url: Option<String>,
}

impl StatementDescriptor {
    /// Builds a descriptor from its required merchant-name text.
    ///
    /// Add the optional phone and URL with the `with_*` builders.
    pub fn new(name: impl Into<String>) -> Result<Self, StatementDescriptorError> {
        let name = normalize_field(
            Some(name.into()),
            MAX_STATEMENT_DESCRIPTOR_NAME_BYTES,
            || StatementDescriptorError::NameTooLong,
        )?
        .ok_or(StatementDescriptorError::MissingName)?;
        Ok(Self {
            name,
            phone: None,
            url: None,
        })
    }

    /// Replaces the customer-service phone; a blank value removes it.
    pub fn with_phone(mut self, phone: Option<String>) -> Result<Self, StatementDescriptorError> {
        let phone = normalize_field(phone, MAX_STATEMENT_DESCRIPTOR_PHONE_BYTES, || {
            StatementDescriptorError::PhoneTooLong
        })?;
        if let Some(phone) = &phone
            && !phone.bytes().all(|byte| {
                byte.is_ascii_digit() || matches!(byte, b' ' | b'+' | b'-' | b'(' | b')' | b'.')
            })
        {
            return Err(StatementDescriptorError::InvalidPhone);
        }
        self.phone = phone;
        Ok(self)
    }

    /// Replaces the merchant URL; a blank value removes it.
    pub fn with_url(mut self, url: Option<String>) -> Result<Self, StatementDescriptorError> {
        let url = normalize_field(url, MAX_STATEMENT_DESCRIPTOR_URL_BYTES, || {
            StatementDescriptorError::UrlTooLong
        })?;
        if let Some(url) = &url
            && url.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(StatementDescriptorError::InvalidUrl);
        }
        self.url = url;
        Ok(self)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn phone(&self) -> Option<&str> {
        self.phone.as_deref()
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
}

fn normalize_field(
    value: Option<String>,
    max_bytes: usize,
    too_long: impl FnOnce() -> StatementDescriptorError,
) -> Result<Option<String>, StatementDescriptorError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max_bytes {
        return Err(too_long());
    }
    if !value.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(StatementDescriptorError::UnsupportedCharacter);
    }
    Ok(Some(value.to_owned()))
}

/// Host policy choosing the statement descriptor for each sale a resolved
/// gateway submits.
///
/// The default descriptor applies to every sale. The optional override applies
/// only to a merchant-initiated recurring sale whose intent marks the first
/// charge after a paid trial, which lets a host print the trial wording the
/// card networks expect on that one charge. A sale that already carries an
/// explicit descriptor is never changed by the policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatementDescriptorPolicy {
    default: StatementDescriptor,
    first_charge_after_paid_trial: Option<StatementDescriptor>,
}

impl StatementDescriptorPolicy {
    pub const fn new(default: StatementDescriptor) -> Self {
        Self {
            default,
            first_charge_after_paid_trial: None,
        }
    }

    /// Sets the descriptor for the first recurring charge after a paid trial.
    pub fn with_first_charge_after_paid_trial(mut self, descriptor: StatementDescriptor) -> Self {
        self.first_charge_after_paid_trial = Some(descriptor);
        self
    }

    pub const fn default_descriptor(&self) -> &StatementDescriptor {
        &self.default
    }

    pub const fn first_charge_after_paid_trial(&self) -> Option<&StatementDescriptor> {
        self.first_charge_after_paid_trial.as_ref()
    }

    /// Selects the descriptor for a sale with the given intent.
    pub const fn descriptor_for(&self, intent: &GatewaySaleIntent) -> &StatementDescriptor {
        match (intent, &self.first_charge_after_paid_trial) {
            (
                GatewaySaleIntent::RecurringStoredCredential {
                    first_charge_after_paid_trial: true,
                    ..
                },
                Some(descriptor),
            ) => descriptor,
            _ => &self.default,
        }
    }
}

impl fmt::Display for StatementDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GatewayPaymentMethodReference, GatewayTransactionId, PaymentToken};

    fn recurring(first_charge_after_paid_trial: bool) -> GatewaySaleIntent {
        GatewaySaleIntent::RecurringStoredCredential {
            payment_method_reference: GatewayPaymentMethodReference::new("vault_1").unwrap(),
            initial_transaction_id: GatewayTransactionId::new("txn_1").unwrap(),
            first_charge_after_paid_trial,
        }
    }

    #[test]
    fn descriptor_trims_bounds_and_normalizes_every_field() {
        let descriptor = StatementDescriptor::new("  IDENTITYPRO.COM ")
            .unwrap()
            .with_phone(Some(" (800) 555-0199 ".to_owned()))
            .unwrap()
            .with_url(Some(" identitypro.com ".to_owned()))
            .unwrap();
        assert_eq!(descriptor.name(), "IDENTITYPRO.COM");
        assert_eq!(descriptor.phone(), Some("(800) 555-0199"));
        assert_eq!(descriptor.url(), Some("identitypro.com"));
        assert_eq!(descriptor.to_string(), "IDENTITYPRO.COM");
        assert!(format!("{descriptor:?}").contains("IDENTITYPRO.COM"));

        let cleared = descriptor
            .clone()
            .with_phone(Some("   ".to_owned()))
            .unwrap()
            .with_url(None)
            .unwrap();
        assert_eq!(cleared.phone(), None);
        assert_eq!(cleared.url(), None);
    }

    #[test]
    fn descriptor_rejects_missing_oversized_and_non_ascii_text() {
        assert_eq!(
            StatementDescriptor::new("   ").unwrap_err(),
            StatementDescriptorError::MissingName
        );
        assert_eq!(
            StatementDescriptor::new("A".repeat(MAX_STATEMENT_DESCRIPTOR_NAME_BYTES + 1))
                .unwrap_err(),
            StatementDescriptorError::NameTooLong
        );
        assert!(StatementDescriptor::new("A".repeat(MAX_STATEMENT_DESCRIPTOR_NAME_BYTES)).is_ok());
        assert_eq!(
            StatementDescriptor::new("IDENTITÉ").unwrap_err(),
            StatementDescriptorError::UnsupportedCharacter
        );
        assert_eq!(
            StatementDescriptor::new("TRIAL\tEND").unwrap_err(),
            StatementDescriptorError::UnsupportedCharacter
        );
        let descriptor = StatementDescriptor::new("IDENTITYPRO").unwrap();
        assert_eq!(
            descriptor
                .clone()
                .with_phone(Some("1".repeat(MAX_STATEMENT_DESCRIPTOR_PHONE_BYTES + 1)))
                .unwrap_err(),
            StatementDescriptorError::PhoneTooLong
        );
        assert_eq!(
            descriptor
                .clone()
                .with_phone(Some("800-555-HELP".to_owned()))
                .unwrap_err(),
            StatementDescriptorError::InvalidPhone
        );
        assert_eq!(
            descriptor
                .clone()
                .with_url(Some("a".repeat(MAX_STATEMENT_DESCRIPTOR_URL_BYTES + 1)))
                .unwrap_err(),
            StatementDescriptorError::UrlTooLong
        );
        assert_eq!(
            descriptor
                .with_url(Some("identity pro.com".to_owned()))
                .unwrap_err(),
            StatementDescriptorError::InvalidUrl
        );
    }

    #[test]
    fn policy_overrides_only_the_first_recurring_charge_after_a_paid_trial() {
        let default = StatementDescriptor::new("IDENTITYPRO.COM").unwrap();
        let trial_end = StatementDescriptor::new("IDENTITYPRO TRIAL").unwrap();
        let one_time = GatewaySaleIntent::OneTime {
            payment_token: PaymentToken::new("tok_1").unwrap(),
        };
        let initial = GatewaySaleIntent::InitialStoredCredential {
            payment_token: PaymentToken::new("tok_2").unwrap(),
        };

        let policy = StatementDescriptorPolicy::new(default.clone());
        for intent in [&one_time, &initial, &recurring(false), &recurring(true)] {
            assert_eq!(policy.descriptor_for(intent), &default);
        }
        assert_eq!(policy.first_charge_after_paid_trial(), None);

        let policy = policy.with_first_charge_after_paid_trial(trial_end.clone());
        assert_eq!(policy.default_descriptor(), &default);
        assert_eq!(policy.first_charge_after_paid_trial(), Some(&trial_end));
        for intent in [&one_time, &initial, &recurring(false)] {
            assert_eq!(policy.descriptor_for(intent), &default);
        }
        assert_eq!(policy.descriptor_for(&recurring(true)), &trial_end);
    }
}
