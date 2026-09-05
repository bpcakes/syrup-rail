use std::str::FromStr;

/// Conservative approval signals in one processor observation, independent of
/// the authoritative payment decision. Only the provider adapter interprets
/// its protocol. A signal can coexist with an unknown or conflicting decision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessorApprovalEvidence {
    /// The observation could not be classified, or a legacy writer omitted its
    /// classification. Always protects manual review, even if raw fields were
    /// discarded. Empty observations are explicitly `Absent`.
    #[default]
    Unclassified,
    /// The adapter found no approval signal in this observation.
    Absent,
    /// Free-form provider text suggests approval; this blocks manual failure
    /// but is insufficient to identify a processor charge.
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
