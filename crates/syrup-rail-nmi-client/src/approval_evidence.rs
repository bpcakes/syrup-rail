use std::sync::LazyLock;

use regex::Regex;

/// Conservative signals observed before response fields are reduced or discarded.
/// This never overrides the authoritative [`crate::PaymentStatus`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentApprovalEvidence {
    /// No approval signal was present in the inspected observation.
    Absent,
    /// Missing, malformed, or indeterminate decisions prevent certifying absence.
    Unclassified,
    /// At least one free-form text occurrence contains an approval token.
    /// Negated prose remains here because free-form grammar cannot prove absence.
    TextOnly,
    /// At least one structured decision occurrence indicates approval.
    Structured,
}

impl PaymentApprovalEvidence {
    pub(crate) const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Structured, _) | (_, Self::Structured) => Self::Structured,
            (Self::TextOnly, _) | (_, Self::TextOnly) => Self::TextOnly,
            (Self::Unclassified, _) | (_, Self::Unclassified) => Self::Unclassified,
            (Self::Absent, Self::Absent) => Self::Absent,
        }
    }
}

static APPROVED_WORD: LazyLock<Regex> = LazyLock::new(|| {
    // This is deliberately a token detector, not a natural-language decision
    // parser. Treating phrases such as "not-approved" as certified absence
    // could turn unfamiliar or compound provider prose into a false negative.
    Regex::new(r"(?i)(^|[^[:alnum:]])approved([^[:alnum:]]|$)")
        .expect("static approval hint expression is valid")
});

pub(crate) fn text_suggests_approval(value: &str) -> bool {
    APPROVED_WORD.is_match(value)
}
