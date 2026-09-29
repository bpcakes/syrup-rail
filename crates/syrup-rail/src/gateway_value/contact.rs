use std::fmt;

use thiserror::Error;

use super::MAX_BILLING_CONTACT_FIELD_BYTES;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BillingContactError {
    #[error("billing contact has no fields")]
    Empty,
    #[error("billing contact field exceeds 255 bytes")]
    FieldTooLong,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BillingAddressError {
    #[error("billing address line 1 is required")]
    MissingLine1,
    #[error("billing address country must be two ASCII letters")]
    InvalidCountry,
    #[error("billing address field exceeds 255 bytes")]
    FieldTooLong,
    #[error("billing address field contains a control character")]
    ControlCharacter,
}

/// A customer-confirmed billing address carried with a payment request.
///
/// Every field is trimmed. `line1` and `country` are required; empty optional
/// fields become absent. Fields are limited to
/// [`MAX_BILLING_CONTACT_FIELD_BYTES`] and may not contain control characters.
/// `country` must be two ASCII letters and is stored uppercase; the postal code
/// remains text so leading zeros are preserved. The type is country-neutral and
/// does not validate a region or postal-code format.
///
/// Provider adapters can enforce stricter formats. The NMI adapter, for
/// example, limits address lines, city, region and postal code more tightly.
/// A request whose address the configured adapter rejects fails before
/// submission after its attempt has been reserved, and a corrected retry needs
/// a new idempotency key, so hosts must pre-validate addresses to their
/// adapter's documented rules. Local validation never proves that a provider
/// or processor will accept the address. Ordinary formatting is value-free.
#[derive(Clone, Eq, PartialEq)]
pub struct BillingAddress {
    line1: String,
    line2: Option<String>,
    city: Option<String>,
    region: Option<String>,
    postal_code: Option<String>,
    country: String,
}

impl BillingAddress {
    /// Builds an address from its required first line and country code.
    ///
    /// Add optional fields with the `with_*` builders.
    pub fn new(line1: String, country: String) -> Result<Self, BillingAddressError> {
        let line1 =
            normalize_address_field(Some(line1))?.ok_or(BillingAddressError::MissingLine1)?;
        let country = country.trim();
        if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(BillingAddressError::InvalidCountry);
        }
        Ok(Self {
            line1,
            line2: None,
            city: None,
            region: None,
            postal_code: None,
            country: country.to_ascii_uppercase(),
        })
    }

    pub fn with_line2(mut self, line2: Option<String>) -> Result<Self, BillingAddressError> {
        self.line2 = normalize_address_field(line2)?;
        Ok(self)
    }

    pub fn with_city(mut self, city: Option<String>) -> Result<Self, BillingAddressError> {
        self.city = normalize_address_field(city)?;
        Ok(self)
    }

    pub fn with_region(mut self, region: Option<String>) -> Result<Self, BillingAddressError> {
        self.region = normalize_address_field(region)?;
        Ok(self)
    }

    pub fn with_postal_code(
        mut self,
        postal_code: Option<String>,
    ) -> Result<Self, BillingAddressError> {
        self.postal_code = normalize_address_field(postal_code)?;
        Ok(self)
    }

    pub fn line1(&self) -> &str {
        &self.line1
    }

    pub fn line2(&self) -> Option<&str> {
        self.line2.as_deref()
    }

    pub fn city(&self) -> Option<&str> {
        self.city.as_deref()
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub fn postal_code(&self) -> Option<&str> {
        self.postal_code.as_deref()
    }

    /// Returns the uppercase two-letter country code.
    pub fn country(&self) -> &str {
        &self.country
    }
}

impl fmt::Debug for BillingAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingAddress")
            .field("has_line2", &self.line2.is_some())
            .field("has_city", &self.city.is_some())
            .field("has_region", &self.region.is_some())
            .field("has_postal_code", &self.postal_code.is_some())
            .finish()
    }
}

impl fmt::Display for BillingAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Normalized contact fields returned by [`BillingContact::into_parts`].
///
/// Destructure it exhaustively so a caller that rebuilds a contact must decide
/// what to do with every field, including the address.
#[derive(Clone, Eq, PartialEq)]
pub struct BillingContactParts {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub address: Option<BillingAddress>,
}

impl fmt::Debug for BillingContactParts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContactParts")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_email", &self.email.is_some())
            .field("has_address", &self.address.is_some())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct BillingContact {
    first_name: Option<String>,
    last_name: Option<String>,
    email: Option<String>,
    address: Option<BillingAddress>,
}

impl BillingContact {
    /// Builds a contact from names and email; at least one must be present.
    ///
    /// Attach a billing address with [`Self::with_address`], or use
    /// [`Self::from_address`] for an address-only contact.
    pub fn new(
        first_name: Option<String>,
        last_name: Option<String>,
        email: Option<String>,
    ) -> Result<Self, BillingContactError> {
        let first_name = normalize_contact_field(first_name)?;
        let last_name = normalize_contact_field(last_name)?;
        let email = normalize_contact_field(email)?;
        if first_name.is_none() && last_name.is_none() && email.is_none() {
            return Err(BillingContactError::Empty);
        }
        Ok(Self {
            first_name,
            last_name,
            email,
            address: None,
        })
    }

    /// Builds a contact that carries only a billing address.
    pub fn from_address(address: BillingAddress) -> Self {
        Self {
            first_name: None,
            last_name: None,
            email: None,
            address: Some(address),
        }
    }

    /// Attaches a billing address, replacing any address already present.
    pub fn with_address(mut self, address: BillingAddress) -> Self {
        self.address = Some(address);
        self
    }

    pub fn first_name(&self) -> Option<&str> {
        self.first_name.as_deref()
    }

    pub fn last_name(&self) -> Option<&str> {
        self.last_name.as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub const fn address(&self) -> Option<&BillingAddress> {
        self.address.as_ref()
    }

    pub fn into_parts(self) -> BillingContactParts {
        BillingContactParts {
            first_name: self.first_name,
            last_name: self.last_name,
            email: self.email,
            address: self.address,
        }
    }
}

impl fmt::Debug for BillingContact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContact")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_email", &self.email.is_some())
            .field("has_address", &self.address.is_some())
            .finish()
    }
}

impl fmt::Display for BillingContact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

fn normalize_contact_field(value: Option<String>) -> Result<Option<String>, BillingContactError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_BILLING_CONTACT_FIELD_BYTES {
        return Err(BillingContactError::FieldTooLong);
    }
    Ok(Some(value.to_owned()))
}

fn normalize_address_field(value: Option<String>) -> Result<Option<String>, BillingAddressError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_BILLING_CONTACT_FIELD_BYTES {
        return Err(BillingAddressError::FieldTooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(BillingAddressError::ControlCharacter);
    }
    Ok(Some(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_contact_normalizes_fields_and_redacts_formatting() {
        let contact = BillingContact::new(
            Some(" Ada ".to_owned()),
            Some(" ".to_owned()),
            Some(" ada@example.com ".to_owned()),
        )
        .unwrap();
        assert_eq!(contact.first_name(), Some("Ada"));
        assert_eq!(contact.last_name(), None);
        assert_eq!(contact.email(), Some("ada@example.com"));
        let debug = format!("{contact:?}");
        assert!(debug.contains("has_first_name: true"));
        assert!(!debug.contains("Ada"));
        assert_eq!(
            BillingContact::new(None, Some(" ".to_owned()), None),
            Err(BillingContactError::Empty)
        );
    }

    fn address(line1: &str, country: &str) -> BillingAddress {
        BillingAddress::new(line1.to_owned(), country.to_owned()).unwrap()
    }

    #[test]
    fn billing_address_normalizes_fields_and_preserves_postal_text() {
        let address = BillingAddress::new(" 1 Main St ".to_owned(), " us ".to_owned())
            .unwrap()
            .with_line2(Some("  ".to_owned()))
            .unwrap()
            .with_city(Some(" Boston ".to_owned()))
            .unwrap()
            .with_region(Some(" MA ".to_owned()))
            .unwrap()
            .with_postal_code(Some(" 02110 ".to_owned()))
            .unwrap();

        assert_eq!(address.line1(), "1 Main St");
        assert_eq!(address.line2(), None);
        assert_eq!(address.city(), Some("Boston"));
        assert_eq!(address.region(), Some("MA"));
        assert_eq!(address.postal_code(), Some("02110"));
        assert_eq!(address.country(), "US");

        let cleared = address.clone().with_city(None).unwrap();
        assert_eq!(cleared.city(), None);
        assert_ne!(cleared, address);
    }

    #[test]
    fn billing_address_rejects_missing_invalid_oversized_and_control_fields() {
        assert_eq!(
            BillingAddress::new("  ".to_owned(), "US".to_owned()),
            Err(BillingAddressError::MissingLine1)
        );
        for country in ["", "U", "USA", "1A", "Ü", "ÜS", "U S"] {
            assert_eq!(
                BillingAddress::new("1 Main St".to_owned(), country.to_owned()),
                Err(BillingAddressError::InvalidCountry),
                "{country:?} must be rejected"
            );
        }

        let at_limit = "a".repeat(MAX_BILLING_CONTACT_FIELD_BYTES);
        let over_limit = "a".repeat(MAX_BILLING_CONTACT_FIELD_BYTES + 1);
        let multibyte_over_limit = "é".repeat(MAX_BILLING_CONTACT_FIELD_BYTES / 2 + 1);
        assert!(multibyte_over_limit.chars().count() < MAX_BILLING_CONTACT_FIELD_BYTES);
        assert_eq!(
            BillingAddress::new(at_limit.clone(), "US".to_owned())
                .unwrap()
                .line1()
                .len(),
            MAX_BILLING_CONTACT_FIELD_BYTES
        );
        assert_eq!(
            BillingAddress::new(over_limit.clone(), "US".to_owned()),
            Err(BillingAddressError::FieldTooLong)
        );
        assert_eq!(
            BillingAddress::new(multibyte_over_limit.clone(), "US".to_owned()),
            Err(BillingAddressError::FieldTooLong)
        );
        let base = address("1 Main St", "US");
        assert!(base.clone().with_line2(Some(at_limit)).is_ok());
        assert_eq!(
            base.clone().with_line2(Some(over_limit.clone())),
            Err(BillingAddressError::FieldTooLong)
        );
        assert_eq!(
            base.clone().with_city(Some(multibyte_over_limit)),
            Err(BillingAddressError::FieldTooLong)
        );
        assert_eq!(
            base.clone().with_region(Some(over_limit.clone())),
            Err(BillingAddressError::FieldTooLong)
        );
        assert_eq!(
            base.clone().with_postal_code(Some(over_limit)),
            Err(BillingAddressError::FieldTooLong)
        );

        for control in [
            "1 Main\nSt",
            "1 Main\tSt",
            "1 Main\u{7f}St",
            "1 Main\u{85}St",
        ] {
            assert_eq!(
                BillingAddress::new(control.to_owned(), "US".to_owned()),
                Err(BillingAddressError::ControlCharacter),
                "{control:?} must be rejected"
            );
            assert_eq!(
                base.clone().with_city(Some(control.to_owned())),
                Err(BillingAddressError::ControlCharacter)
            );
        }
    }

    #[test]
    fn billing_address_and_contact_formatting_is_value_free() {
        let address = address("1 Secret St", "US")
            .with_postal_code(Some("99999".to_owned()))
            .unwrap();
        let debug = format!("{address:?}");
        assert!(debug.contains("has_postal_code: true"));
        assert!(debug.contains("has_city: false"));
        for sentinel in ["Secret", "99999", "US"] {
            assert!(!debug.contains(sentinel), "{sentinel} leaked into {debug}");
        }
        assert_eq!(address.to_string(), "[redacted]");

        let contact = BillingContact::new(Some("Ada".to_owned()), None, None)
            .unwrap()
            .with_address(address.clone());
        let debug = format!("{contact:?}");
        assert!(debug.contains("has_address: true"));
        assert!(!debug.contains("Secret"));
        let parts = format!("{:?}", contact.into_parts());
        assert!(parts.contains("has_address: true"));
        assert!(!parts.contains("Ada"));
        assert!(!parts.contains("Secret"));
    }

    #[test]
    fn billing_contact_carries_address_only_or_alongside_names() {
        let home = address("1 Main St", "US");
        let office = address("2 Side St", "CA");

        let address_only = BillingContact::from_address(home.clone());
        assert_eq!(address_only.first_name(), None);
        assert_eq!(address_only.last_name(), None);
        assert_eq!(address_only.email(), None);
        assert_eq!(address_only.address(), Some(&home));

        let named = BillingContact::new(
            Some("Ada".to_owned()),
            Some("Lovelace".to_owned()),
            Some("ada@example.test".to_owned()),
        )
        .unwrap();
        assert_eq!(named.address(), None);
        let replaced = named.with_address(home).with_address(office.clone());
        assert_eq!(replaced.address(), Some(&office));

        let BillingContactParts {
            first_name,
            last_name,
            email,
            address,
        } = replaced.into_parts();
        assert_eq!(first_name.as_deref(), Some("Ada"));
        assert_eq!(last_name.as_deref(), Some("Lovelace"));
        assert_eq!(email.as_deref(), Some("ada@example.test"));
        assert_eq!(address, Some(office));

        assert_eq!(
            BillingContact::new(None, None, None),
            Err(BillingContactError::Empty),
            "names/email constructor semantics are unchanged"
        );
    }
}
