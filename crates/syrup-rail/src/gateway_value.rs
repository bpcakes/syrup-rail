use std::{fmt, sync::LazyLock};

use regex::Regex;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{raw_card_data_ranges, string_contains_raw_card_data};

pub const MAX_GATEWAY_TEXT_BYTES: usize = 512;
pub const MAX_BILLING_CONTACT_FIELD_BYTES: usize = 255;

macro_rules! impl_redacted_format {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "([redacted])"))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("[redacted]")
            }
        }
    };
}

static SENSITIVE_GATEWAY_FIELD_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?ix)
        ["']?
        (?P<name>
            [a-z0-9_-]*
            (?:
                payment[_-]?token
                | token
                | card[_-]?number
                | cc[_-]?number
                | cc[_-]?exp
                | pan
                | expiration(?:date)?
                | card[_-]?code
                | card[_-]?verification
                | verification[_-]?value
                | cvv
                | cvv2
                | cvc
                | cvn
                | magstripe
                | track[_-]?[12]?
                | emv
                | dukpt
                | ksn
                | cavv
                | three[_-]?ds[a-z0-9_-]*
                | security[_-]?key
                | signature
                | sig
                | hmac
                | mac
                | session[_-]?(?:key|id)
                | passwd
                | password
                | pin
                | otp
                | mfa
                | access[_-]?key
                | api[_-]?key
                | secret
                | webhook[_-]?secret
                | authorization
                | bearer
                | customer[_-]?vault(?:[_-]?id)?
                | vault[_-]?id
                | (?:nmi[_-]?|merchant[_-]?)?order[_-]?id
                | (?:nmi[_-]?|processor[_-]?)?transaction[_-]?id
                | txn[_-]?id
                | check[_-]?account
                | check[_-]?aba
                | check[_-]?name
                | avs
                | routing[_-]?number
                | account[_-]?number
                | iban
                | swift
                | ssn
                | tax[_-]?id
                | dob
                | date[_-]?of[_-]?birth
                | (?:billing|customer)[_-]?email
                | (?:billing|customer|cardholder)[_-]?first[_-]?name
                | (?:billing|customer|cardholder)[_-]?last[_-]?name
                | billing[_-]?name
                | cardholder[_-]?name
                | customer[_-]?name
                | (?:billing|customer)[_-]?phone(?:[_-]?number)?
                | telephone
                | (?:billing|customer)[_-]?zip(?:[_-]?code)?
                | (?:billing|customer)[_-]?postal[_-]?code
                | (?:billing|shipping|customer)[_-]?address(?:[_-]?line[_-]?[12])?
                | (?:billing|shipping|customer)[_-]?street(?:[_-]?address)?
                | (?:billing|shipping|customer)[_-]?city
                | (?:billing|shipping|customer)[_-]?state
                | (?:billing|shipping|customer)[_-]?country
            )
            [a-z0-9_-]*
        )
        ["']?
        \s*[:=]\s*
        "#,
    )
    .expect("gateway sensitive field prefix regex should compile")
});

static AUTHORIZATION_SCHEME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?ix)
        ["']?
        (?P<name>[a-z0-9_-]*authorization[a-z0-9_-]*)
        ["']?
        \s*[:=]\s*
        (?:bearer|basic)\s+
        [^"',&\s;{}]+
        "#,
    )
    .expect("gateway authorization scheme regex should compile")
});

static JWT_LIKE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
        .expect("JWT-like regex should compile")
});

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PaymentTokenError {
    #[error("payment token is empty")]
    Empty,
    #[error("payment token exceeds 512 bytes")]
    TooLong,
    #[error("payment token contains raw card data")]
    RawCardData,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PaymentToken(Zeroizing<String>);

impl PaymentToken {
    pub fn new(value: impl Into<String>) -> Result<Self, PaymentTokenError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(PaymentTokenError::Empty);
        }
        if value.len() > MAX_GATEWAY_TEXT_BYTES {
            return Err(PaymentTokenError::TooLong);
        }
        if string_contains_raw_card_data(&value) {
            return Err(PaymentTokenError::RawCardData);
        }
        Ok(Self(Zeroizing::new(value)))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_inner(mut self) -> String {
        std::mem::take(&mut *self.0)
    }
}

impl_redacted_format!(PaymentToken);

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum GatewayReferenceValueError {
    #[error("gateway reference is empty")]
    Empty,
    #[error("gateway reference exceeds 512 bytes")]
    TooLong,
    #[error("gateway reference contains raw card data")]
    RawCardData,
    #[error("gateway correlation identifier contains unsupported characters")]
    UnsupportedCorrelationCharacter,
    #[error("generated gateway order ID does not contain the expected attempt identity")]
    GeneratedOrderAttemptMismatch,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct GatewayReferenceValue(String);

impl GatewayReferenceValue {
    fn new(value: impl Into<String>) -> Result<Self, GatewayReferenceValueError> {
        Self::new_with_trim(value.into(), str::trim)
    }

    fn new_transaction(value: impl Into<String>) -> Result<Self, GatewayReferenceValueError> {
        Self::new_with_trim(value.into(), trim_gateway_transaction_id)
    }

    fn new_generated_order(
        value: impl Into<String>,
        attempt_id: crate::PaymentAttemptId,
    ) -> Result<Self, GatewayReferenceValueError> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(GatewayReferenceValueError::Empty);
        }
        if value.len() > MAX_GATEWAY_TEXT_BYTES {
            return Err(GatewayReferenceValueError::TooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(GatewayReferenceValueError::UnsupportedCorrelationCharacter);
        }

        let attempt_suffix = attempt_id.as_uuid().simple().to_string();
        let Some(prefix) = value.strip_suffix(&attempt_suffix) else {
            return Err(GatewayReferenceValueError::GeneratedOrderAttemptMismatch);
        };
        if !matches!(prefix.as_bytes().last(), Some(b'_' | b'-' | b'.')) {
            return Err(GatewayReferenceValueError::GeneratedOrderAttemptMismatch);
        }
        if string_contains_raw_card_data(prefix) {
            return Err(GatewayReferenceValueError::RawCardData);
        }
        Ok(Self(value.to_owned()))
    }

    fn new_correlation(value: &str) -> Result<Self, GatewayReferenceValueError> {
        let value = value.trim();
        let parsed = Self::new(value)?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(GatewayReferenceValueError::UnsupportedCorrelationCharacter);
        }
        Ok(parsed)
    }

    fn new_with_trim(
        value: String,
        trim: impl FnOnce(&str) -> &str,
    ) -> Result<Self, GatewayReferenceValueError> {
        let value = trim(&value);
        if value.is_empty() {
            return Err(GatewayReferenceValueError::Empty);
        }
        if value.len() > MAX_GATEWAY_TEXT_BYTES {
            return Err(GatewayReferenceValueError::TooLong);
        }
        if string_contains_raw_card_data(value) {
            return Err(GatewayReferenceValueError::RawCardData);
        }
        Ok(Self(value.to_owned()))
    }

    fn expose(&self) -> &str {
        &self.0
    }

    fn into_inner(self) -> String {
        self.0
    }
}

macro_rules! gateway_reference_type {
    ($name:ident, $constructor:ident) => {
        #[derive(Clone, Eq, Hash, PartialEq)]
        pub struct $name(GatewayReferenceValue);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, GatewayReferenceValueError> {
                GatewayReferenceValue::$constructor(value).map(Self)
            }

            pub fn from_correlation(value: &str) -> Result<Self, GatewayReferenceValueError> {
                GatewayReferenceValue::new_correlation(value).map(Self)
            }

            pub fn expose(&self) -> &str {
                self.0.expose()
            }

            pub fn into_inner(self) -> String {
                self.0.into_inner()
            }
        }

        impl_redacted_format!($name);
    };
}

gateway_reference_type!(GatewayTransactionId, new_transaction);
gateway_reference_type!(GatewayPaymentMethodReference, new);

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct GatewayOrderId(GatewayReferenceValue);

impl GatewayOrderId {
    /// Constructs an order ID produced from a specific payment attempt.
    ///
    /// The value must end with that attempt's simple UUID after an identifier
    /// separator. The UUID is exempt from the raw-card heuristic because random
    /// UUID digit runs can be Luhn-valid by coincidence; the remaining prefix is
    /// still scanned. Untrusted provider text must use [`Self::from_correlation`]
    /// unless its adapter has proved the generated grammar and recovered this
    /// exact attempt identity from it.
    pub fn from_generated_attempt(
        value: impl Into<String>,
        attempt_id: crate::PaymentAttemptId,
    ) -> Result<Self, GatewayReferenceValueError> {
        GatewayReferenceValue::new_generated_order(value, attempt_id).map(Self)
    }

    pub fn from_correlation(value: &str) -> Result<Self, GatewayReferenceValueError> {
        GatewayReferenceValue::new_correlation(value).map(Self)
    }

    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    pub fn into_inner(self) -> String {
        self.0.into_inner()
    }
}

impl_redacted_format!(GatewayOrderId);

const fn is_gateway_transaction_id_edge_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{0085}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

fn trim_gateway_transaction_id(value: &str) -> &str {
    value.trim_matches(is_gateway_transaction_id_edge_whitespace)
}

pub fn canonical_gateway_transaction_id(value: Option<&str>) -> Option<&str> {
    value
        .map(trim_gateway_transaction_id)
        .filter(|value| !value.is_empty())
}

pub fn canonical_gateway_transaction_ids_equal(left: &str, right: &str) -> bool {
    canonical_gateway_transaction_id(Some(left)).is_some_and(|left| {
        canonical_gateway_transaction_id(Some(right)).is_some_and(|right| left == right)
    })
}

#[derive(Clone, Eq, PartialEq)]
pub struct GatewayDiagnostic(String);

impl GatewayDiagnostic {
    pub fn new(message: &str) -> Self {
        Self(sanitize_gateway_detail(message))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl_redacted_format!(GatewayDiagnostic);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum BillingContactError {
    #[error("billing contact has no fields")]
    Empty,
    #[error("billing contact field exceeds 255 bytes")]
    FieldTooLong,
}

#[derive(Clone, Eq, PartialEq)]
pub struct BillingContact {
    first_name: Option<String>,
    last_name: Option<String>,
    email: Option<String>,
}

impl BillingContact {
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
        })
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

    pub fn into_parts(self) -> (Option<String>, Option<String>, Option<String>) {
        (self.first_name, self.last_name, self.email)
    }
}

impl fmt::Debug for BillingContact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BillingContact")
            .field("has_first_name", &self.first_name.is_some())
            .field("has_last_name", &self.last_name.is_some())
            .field("has_email", &self.email.is_some())
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

pub fn sanitize_gateway_detail(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted_jwt = JWT_LIKE_RE.replace_all(&normalized, "[redacted]");
    let redacted_pan = redact_raw_card_data(&redacted_jwt);
    let redacted_authorization =
        AUTHORIZATION_SCHEME_RE.replace_all(&redacted_pan, "$name=[redacted]");
    let redacted_fields = redact_gateway_fields(&redacted_authorization);
    truncate_gateway_detail_to_length(redacted_fields.trim(), MAX_GATEWAY_TEXT_BYTES)
}

fn redact_raw_card_data(message: &str) -> String {
    let ranges = raw_card_data_ranges(message);
    if ranges.is_empty() {
        return message.to_owned();
    }
    let mut redacted = String::with_capacity(message.len());
    let mut cursor = 0;
    for range in ranges {
        redacted.push_str(&message[cursor..range.start]);
        redacted.push_str("[redacted]");
        cursor = range.end;
    }
    redacted.push_str(&message[cursor..]);
    redacted
}

fn redact_gateway_fields(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut cursor = 0;
    let mut search_start = 0;

    while search_start < message.len() {
        let Some(captures) = SENSITIVE_GATEWAY_FIELD_PREFIX_RE.captures(&message[search_start..])
        else {
            break;
        };
        let Some(match_) = captures.get(0) else {
            break;
        };
        let Some(name) = captures.name("name") else {
            break;
        };
        let match_start = search_start + match_.start();
        let value_start = search_start + match_.end();
        let value_end = consume_gateway_field_value(message, value_start);

        redacted.push_str(&message[cursor..match_start]);
        redacted.push_str(name.as_str());
        redacted.push_str("=[redacted]");
        cursor = value_end;
        search_start = value_end;
    }

    redacted.push_str(&message[cursor..]);
    redacted
}

fn consume_gateway_field_value(message: &str, start: usize) -> usize {
    let Some((_, first)) = message[start..].char_indices().next() else {
        return start;
    };
    match first {
        '"' | '\'' => consume_quoted_gateway_value(message, start, first),
        '{' | '[' => consume_balanced_gateway_value(message, start, first),
        _ => consume_bare_gateway_value(message, start),
    }
}

fn consume_quoted_gateway_value(message: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (offset, character) in message[start..].char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            return start + offset + character.len_utf8();
        }
    }
    message.len()
}

fn consume_balanced_gateway_value(message: &str, start: usize, opener: char) -> usize {
    let mut stack = vec![opener];
    let mut quoted_by = None;
    let mut escaped = false;

    for (offset, character) in message[start..].char_indices().skip(1) {
        if let Some(quote) = quoted_by {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
                continue;
            }
            if character == quote {
                quoted_by = None;
            }
            continue;
        }

        match character {
            '"' | '\'' => quoted_by = Some(character),
            '{' | '[' => stack.push(character),
            '}' if stack.last() == Some(&'{') => {
                stack.pop();
                if stack.is_empty() {
                    return start + offset + character.len_utf8();
                }
            }
            ']' if stack.last() == Some(&'[') => {
                stack.pop();
                if stack.is_empty() {
                    return start + offset + character.len_utf8();
                }
            }
            '}' | ']' => return start + offset,
            _ => {}
        }
    }
    message.len()
}

fn consume_bare_gateway_value(message: &str, start: usize) -> usize {
    for (offset, character) in message[start..].char_indices() {
        if character.is_whitespace()
            || matches!(
                character,
                '"' | '\'' | ',' | '&' | ';' | '{' | '}' | '[' | ']'
            )
        {
            return start + offset;
        }
    }
    message.len()
}

pub fn truncate_gateway_detail_to_length(message: &str, max_length: usize) -> String {
    if max_length == 0 {
        return String::new();
    }
    if message.len() <= max_length {
        return message.to_owned();
    }
    if max_length <= 3 {
        return ".".repeat(max_length);
    }
    let max_prefix_len = max_length.saturating_sub(3);
    let mut truncated = String::new();
    for character in message.chars() {
        if truncated.len() + character.len_utf8() > max_prefix_len {
            break;
        }
        truncated.push(character);
    }
    trim_partial_redaction_marker_suffix(&mut truncated);
    truncated.push_str("...");
    truncated
}

fn trim_partial_redaction_marker_suffix(value: &mut String) {
    const REDACTION_MARKER: &str = "[redacted]";
    for prefix_len in (1..REDACTION_MARKER.len()).rev() {
        if value.ends_with(&REDACTION_MARKER[..prefix_len]) {
            value.truncate(value.len() - prefix_len);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_token_is_bounded_preserved_and_redacted() {
        let token = PaymentToken::new(" token_value ").unwrap();
        assert_eq!(token.expose(), " token_value ");
        assert_eq!(format!("{token:?}"), "PaymentToken([redacted])");
        assert_eq!(token.to_string(), "[redacted]");
        assert_eq!(PaymentToken::new("  "), Err(PaymentTokenError::Empty));
        assert_eq!(
            PaymentToken::new("x".repeat(MAX_GATEWAY_TEXT_BYTES + 1)),
            Err(PaymentTokenError::TooLong)
        );
        assert_eq!(
            PaymentToken::new("4111111111111111"),
            Err(PaymentTokenError::RawCardData)
        );
    }

    #[test]
    fn generated_order_exempts_only_its_typed_attempt_uuid() {
        let attempt_id: crate::PaymentAttemptId =
            "00000000-0000-0000-0000-000000000000".parse().unwrap();
        let value = "ck_renewal_00000000000000000000000000000000";
        assert!(string_contains_raw_card_data(value));
        assert_eq!(
            GatewayOrderId::from_generated_attempt(value, attempt_id)
                .unwrap()
                .expose(),
            value
        );
        assert_eq!(
            GatewayOrderId::from_generated_attempt(
                "ck_renewal_11111111111111111111111111111111",
                attempt_id,
            ),
            Err(GatewayReferenceValueError::GeneratedOrderAttemptMismatch)
        );
        assert_eq!(
            GatewayOrderId::from_generated_attempt(
                "4111111111111111_00000000000000000000000000000000",
                attempt_id,
            ),
            Err(GatewayReferenceValueError::RawCardData)
        );
    }

    #[test]
    fn correlation_admission_is_strict_but_transaction_canonicalization_is_exact() {
        assert_eq!(
            GatewayTransactionId::from_correlation(" txn_A-1.2 ")
                .unwrap()
                .expose(),
            "txn_A-1.2"
        );
        assert!(GatewayTransactionId::from_correlation("txn A").is_err());
        assert_eq!(
            GatewayTransactionId::new("\u{00a0}TxN ID\u{3000}")
                .unwrap()
                .expose(),
            "TxN ID"
        );
        assert!(canonical_gateway_transaction_ids_equal(
            "\u{00a0}TxN ID",
            "TxN ID\u{3000}"
        ));
        assert!(!canonical_gateway_transaction_ids_equal("Txn", "txn"));
        assert_eq!(
            canonical_gateway_transaction_id(Some("\u{200b}Txn\u{200b}")),
            Some("\u{200b}Txn\u{200b}")
        );
    }

    #[test]
    fn diagnostic_sanitizer_redacts_known_secrets_and_is_byte_bounded() {
        let diagnostic = GatewayDiagnostic::new(
            r#"gateway {"payment_token":"tok_secret","customer_vault_id":"vault_secret"} card 4111 1111 1111 1111"#,
        );
        assert!(diagnostic.expose().contains("payment_token=[redacted]"));
        assert!(diagnostic.expose().contains("customer_vault_id=[redacted]"));
        assert!(!diagnostic.expose().contains("tok_secret"));
        assert!(!diagnostic.expose().contains("vault_secret"));
        assert!(!diagnostic.expose().contains("4111"));
        assert_eq!(format!("{diagnostic:?}"), "GatewayDiagnostic([redacted])");

        let bounded = GatewayDiagnostic::new(&"é".repeat(400));
        assert!(bounded.expose().len() <= MAX_GATEWAY_TEXT_BYTES);
        assert!(bounded.expose().is_char_boundary(bounded.expose().len()));
    }

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

    #[test]
    fn truncation_never_leaves_a_partial_redaction_marker() {
        let truncated = truncate_gateway_detail_to_length("abcdefgh[redacted] trailing", 15);
        assert!(truncated.len() <= 15);
        assert!(truncated.ends_with("..."));
        assert!(!truncated.ends_with("[red..."));
    }
}
