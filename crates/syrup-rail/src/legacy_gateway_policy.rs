//! Deprecated compatibility predicates for the historical gateway vocabulary.
//! Financial policy must consume typed adapter facts instead.
#![allow(deprecated)]

#[deprecated(note = "provider vocabulary belongs in the adapter; use ProcessorApprovalEvidence")]
pub fn gateway_state_is_approved(value: &str) -> bool {
    matches!(
        normalized_gateway_state_value(value).as_str(),
        "approved"
            | "complete"
            | "completed"
            | "captured"
            | "success"
            | "successful"
            | "pendingsettlement"
    )
}

#[deprecated(note = "provider vocabulary belongs in the adapter; use ProcessorApprovalEvidence")]
pub fn gateway_response_is_approved(value: Option<&str>) -> bool {
    let Some(normalized) = value.map(normalized_gateway_state_value) else {
        return false;
    };
    matches!(normalized.as_str(), "1" | "100") || gateway_state_is_approved(&normalized)
}

fn normalized_gateway_state_value(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| {
            !character.is_ascii_whitespace() && *character != '_' && *character != '-'
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_state_matching_preserves_processor_spellings() {
        for value in [
            "approved",
            "complete",
            "completed",
            "captured",
            "success",
            "successful",
            "pending settlement",
            "pending_settlement",
            "pending-settlement",
            "Pending Settlement",
        ] {
            assert!(gateway_state_is_approved(value), "missed {value}");
        }
        assert!(gateway_response_is_approved(Some("1")));
        assert!(gateway_response_is_approved(Some("100")));
        assert!(!gateway_response_is_approved(Some("200")));
        assert!(!gateway_response_is_approved(None));
    }
}
