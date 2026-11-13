use super::*;

const MAX_ACTION_TYPE_CHARS: usize = 32;

#[derive(Default)]
pub(super) struct Evidence {
    pub(super) malformed: bool,
    pub(super) conflicts: Vec<ReceiptConflictField>,
}

impl Evidence {
    pub(super) fn field(
        &mut self,
        node: roxmltree::Node<'_, '_>,
        name: &'static str,
        kind: ReceiptConflictField,
        normalize: fn(&str) -> Option<String>,
    ) -> Option<SensitiveText> {
        let scan = scan(node, name, normalize);
        if scan.conflicting && !self.conflicts.contains(&kind) {
            self.conflicts.push(kind);
        }
        self.malformed |= scan.malformed;
        // Even a valid candidate cannot be trusted as this field's value when
        // another occurrence is malformed. Other fields remain independent.
        if scan.malformed {
            None
        } else {
            scan.selected.map(SensitiveText::new)
        }
    }

    /// Checks a field that is not original-payment evidence. Equivalent
    /// spellings agree like any other field; a real disagreement is ordinary
    /// malformation, never an attributed conflict.
    pub(super) fn non_evidence_field(
        &mut self,
        node: roxmltree::Node<'_, '_>,
        name: &'static str,
        normalize: fn(&str) -> Option<String>,
    ) {
        let scan = scan(node, name, normalize);
        self.malformed |= scan.malformed || scan.conflicting;
    }
}

struct Scan {
    selected: Option<String>,
    malformed: bool,
    conflicting: bool,
}

fn scan(
    node: roxmltree::Node<'_, '_>,
    name: &'static str,
    normalize: fn(&str) -> Option<String>,
) -> Scan {
    let mut scan = Scan {
        selected: None,
        malformed: false,
        conflicting: false,
    };
    let mut empty = false;
    let mut populated = false;
    for child in element_children_named(node, name) {
        let ScalarOccurrence::Scalar(raw) = xml_scalar_occurrence(child) else {
            scan.malformed = true;
            continue;
        };
        // Do not truncate a field into evidence. Retain only one bounded
        // comparison value; invalid occurrences cannot prove a conflict or
        // erase one proved by two other valid occurrences, in any order.
        if raw.chars().count() > crate::client::MAX_NMI_FIELD_CHARS {
            scan.malformed = true;
            continue;
        }
        let raw = raw.trim();
        if raw.is_empty() {
            empty = true;
            continue;
        }
        populated = true;
        let Some(value) = normalize(raw) else {
            scan.malformed = true;
            continue;
        };
        if let Some(previous) = &scan.selected {
            scan.conflicting |= previous != &value;
        } else {
            scan.selected = Some(value);
        }
    }
    scan.malformed |= empty && populated;
    scan
}

pub(super) fn condition(value: &str) -> Option<String> {
    Some(normalize_gateway_state(value))
}

pub(super) fn currency(value: &str) -> Option<String> {
    (value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_alphabetic()))
        .then(|| value.to_ascii_uppercase())
}

pub(super) fn action_type(value: &str) -> Option<String> {
    // Validate the token's shape, not its membership in a known list: an
    // action type this crate has not seen must not make an otherwise valid
    // original payment permanently incomplete. Hosts select the action types
    // they recognize and ignore the rest.
    let normalized = normalize_gateway_state(value);
    (normalized.len() <= MAX_ACTION_TYPE_CHARS
        && normalized
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && normalized
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
    .then_some(normalized)
}

pub(super) fn amount(value: &str) -> Option<String> {
    let normalized = normalize_report_amount(value);
    let unsigned = normalized.strip_prefix('-').unwrap_or(&normalized);
    let (whole, fraction) = unsigned.split_once('.')?;
    if whole.is_empty()
        || fraction.len() != 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // The receipt amount must be representable as signed minor units. The
    // shared report normalizer alone deliberately preserves invalid strings.
    normalized.replace('.', "").parse::<i64>().ok()?;
    Some(normalized)
}

pub(super) fn success(value: &str) -> Option<String> {
    let normalized = normalize_report_success(value);
    matches!(normalized.as_str(), "0" | "1").then_some(normalized)
}

pub(super) fn response_code(value: &str) -> Option<String> {
    (value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_digit())).then(|| value.to_owned())
}

pub(super) fn date(value: &str) -> Option<String> {
    // Query API timestamps are local calendar values in YYYYMMDDhhmmss form.
    // Validate the calendar without adding a timezone or date dependency.
    if value.len() != 14 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year = value[..4].parse::<u16>().ok()?;
    let month = value[4..6].parse::<u8>().ok()?;
    let day = value[6..8].parse::<u8>().ok()?;
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    (year != 0
        && (1..=days).contains(&day)
        && value[8..10].parse::<u8>().ok()? < 24
        && value[10..12].parse::<u8>().ok()? < 60
        && value[12..14].parse::<u8>().ok()? < 60)
        .then(|| value.to_owned())
}
