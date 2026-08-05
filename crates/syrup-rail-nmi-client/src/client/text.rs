use crate::SensitiveText;
use zeroize::Zeroizing;

use super::MAX_NMI_FIELD_CHARS;

pub(super) fn bounded_gateway_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(truncate_gateway_text(value, MAX_NMI_FIELD_CHARS))
}

pub(super) fn bounded_detail(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_gateway_text(&normalized, MAX_NMI_FIELD_CHARS)
}

pub(super) fn parse_provider_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_NMI_FIELD_CHARS {
        return None;
    }
    Some(value.to_owned())
}

pub(super) fn truncate_gateway_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        truncated
    } else {
        value.to_owned()
    }
}

pub(super) fn sensitive_gateway_field(value: Option<String>) -> Option<SensitiveText> {
    value.map(SensitiveText::new)
}

pub(super) fn last4(value: String) -> Option<String> {
    let value = Zeroizing::new(value);
    let has_mask = value
        .chars()
        .any(|char| matches!(char, '*' | 'x' | 'X' | '#'));
    let digits = Zeroizing::new(
        value
            .chars()
            .filter(|char| char.is_ascii_digit())
            .collect::<String>(),
    );
    if digits.len() == 4 || (has_mask && digits.len() >= 4) {
        Some(digits[digits.len() - 4..].to_owned())
    } else {
        None
    }
}

pub(super) fn valid_last4(value: String) -> Option<String> {
    (value.len() == 4 && value.chars().all(|char| char.is_ascii_digit())).then_some(value)
}

pub(super) fn parse_expiry(expiry: Option<&str>) -> (Option<i16>, Option<i16>) {
    let Some(expiry) = expiry else {
        return (None, None);
    };
    let expiry = expiry.trim();
    if expiry.len() == 4 && expiry.chars().all(|char| char.is_ascii_digit()) {
        let month = expiry[0..2].parse::<i16>().ok();
        let year = expiry[2..4].parse::<i16>().ok().map(|value| 2000 + value);
        return valid_expiry(month, year);
    }
    if expiry.len() == 6 && expiry.chars().all(|char| char.is_ascii_digit()) {
        let month = expiry[0..2].parse::<i16>().ok();
        let year = expiry[2..6].parse::<i16>().ok();
        return valid_expiry(month, year);
    }
    let parts: Vec<&str> = expiry.split(['/', '-']).collect();
    if parts.len() != 2 {
        return (None, None);
    }
    let month = parts[0].parse::<i16>().ok();
    let mut year = parts[1].parse::<i16>().ok();
    if let Some(value) = year
        && value < 100
    {
        year = Some(2000 + value);
    }
    valid_expiry(month, year)
}

fn valid_expiry(month: Option<i16>, year: Option<i16>) -> (Option<i16>, Option<i16>) {
    match month {
        Some(month) if (1..=12).contains(&month) => (
            Some(month),
            year.filter(|year| (2000..=2100).contains(year)),
        ),
        _ => (None, None),
    }
}
