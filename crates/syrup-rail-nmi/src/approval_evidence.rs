use syrup_rail::ProcessorApprovalEvidence;
use syrup_rail_nmi_client::{PaymentApprovalEvidence, PaymentOutcomeParts};

// The raw client owns the full lossless observation. Do not reparse its retained
// strings here: status and duplicate occurrences may already have been discarded.
pub(crate) fn classify(parts: &PaymentOutcomeParts) -> ProcessorApprovalEvidence {
    match parts.approval_evidence {
        PaymentApprovalEvidence::Absent => ProcessorApprovalEvidence::Absent,
        PaymentApprovalEvidence::Unclassified => ProcessorApprovalEvidence::Unclassified,
        PaymentApprovalEvidence::TextOnly => ProcessorApprovalEvidence::TextOnly,
        PaymentApprovalEvidence::Structured => ProcessorApprovalEvidence::Structured,
    }
}
