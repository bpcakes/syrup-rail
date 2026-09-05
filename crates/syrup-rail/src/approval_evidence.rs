use std::str::FromStr;

/// Conservative approval signals in one processor observation, independent of
/// the authoritative payment decision. Only the provider adapter interprets
/// its protocol. A signal can coexist with an unknown or conflicting decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessorApprovalEvidence {
    /// The observation could not be classified, or a legacy writer omitted its
    /// classification. Always protects manual review for payment-bearing
    /// attempts, even if raw fields were discarded. Empty observations are
    /// explicitly `Absent`.
    #[default]
    Unclassified,
    /// The adapter found no approval signal in this observation.
    Absent,
    /// Free-form provider text suggests approval; this blocks manual failure for
    /// payment-bearing attempts but is insufficient to identify a processor
    /// charge. Snapshot-guarded zero-value payment-method updates use their
    /// separate manual-closure policy.
    TextOnly,
    /// At least one structured provider field indicates approval. This is not
    /// proof of an authoritative approval when other evidence conflicts.
    Structured,
}

impl ProcessorApprovalEvidence {
    /// Preserve every conservative signal when observations cannot safely
    /// replace one another. This does not merge their identities or raw fields.
    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Structured, _) | (_, Self::Structured) => Self::Structured,
            (Self::TextOnly, _) | (_, Self::TextOnly) => Self::TextOnly,
            (Self::Unclassified, _) | (_, Self::Unclassified) => Self::Unclassified,
            (Self::Absent, Self::Absent) => Self::Absent,
        }
    }

    /// Stable provider-neutral persistence label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::Absent => "absent",
            Self::TextOnly => "text_only",
            Self::Structured => "structured",
        }
    }
}

/// An unknown persisted classification must never silently become absence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("processor approval evidence classification is not recognized")]
pub struct ProcessorApprovalEvidenceParseError;

impl FromStr for ProcessorApprovalEvidence {
    type Err = ProcessorApprovalEvidenceParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unclassified" => Ok(Self::Unclassified),
            "absent" => Ok(Self::Absent),
            "text_only" => Ok(Self::TextOnly),
            "structured" => Ok(Self::Structured),
            _ => Err(ProcessorApprovalEvidenceParseError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessorApprovalEvidence::{Absent, Structured, TextOnly, Unclassified};

    #[test]
    fn merge_is_commutative_and_preserves_the_strongest_conservative_signal() {
        let cases = [
            (Absent, Absent, Absent),
            (Absent, Unclassified, Unclassified),
            (Absent, TextOnly, TextOnly),
            (Absent, Structured, Structured),
            (Unclassified, Unclassified, Unclassified),
            (Unclassified, TextOnly, TextOnly),
            (Unclassified, Structured, Structured),
            (TextOnly, TextOnly, TextOnly),
            (TextOnly, Structured, Structured),
            (Structured, Structured, Structured),
        ];

        for (left, right, expected) in cases {
            assert_eq!(left.merge(right), expected);
            assert_eq!(right.merge(left), expected);
        }
    }
}
