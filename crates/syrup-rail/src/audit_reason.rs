pub(crate) enum ReasonValidationError {
    Empty,
    TooLong,
    ContainsRawCardData,
}

/// Shared audit-text invariant; public reason types retain their own errors and formatting.
pub(crate) fn normalize_audit_reason(
    value: impl Into<String>,
) -> Result<String, ReasonValidationError> {
    let value = value.into();
    let value = value.trim();
    if value.is_empty() {
        return Err(ReasonValidationError::Empty);
    }
    if value.chars().count() > 500 {
        return Err(ReasonValidationError::TooLong);
    }
    if crate::string_contains_raw_card_data(value) {
        return Err(ReasonValidationError::ContainsRawCardData);
    }
    Ok(value.to_owned())
}
