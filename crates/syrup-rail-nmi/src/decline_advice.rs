//! NMI response-code interpretation for automatic-renewal retry advice.
//!
//! Only NMI's normalized `response_code` is interpreted, and only codes whose
//! published meaning says that retrying the same payment method cannot succeed,
//! is not permitted, or must wait. Every other or missing code follows the
//! subscription's dunning schedule.
//! <https://docs.nmi.com/reference/response-codes>
//!
//! NMI's code is a lossy rendering of the issuer's own response code, which is
//! not part of the durable processor evidence. A code stops retries only when
//! every issuer response it is known to cover forbids or defeats a retry. A
//! code that also covers a retryable issuer response follows the schedule:
//! stopping ends the subscription's dunning, while a scheduled retry of a true
//! hard decline costs at most a few declined attempts.

use std::num::NonZeroU32;

use syrup_rail::{DunningRetryDelay, ProcessorEvidence, RenewalDeclineRetryAdvice};

/// Earliest automatic retry after NMI code 264, "Declined-Retry in a few
/// days". NMI publishes no exact delay, so Syrup Rail reads "a few days" as at
/// least three days.
pub const NMI_RETRY_IN_A_FEW_DAYS_DELAY: DunningRetryDelay =
    DunningRetryDelay::from_non_zero_seconds(match NonZeroU32::new(3 * 24 * 60 * 60) {
        Some(seconds) => seconds,
        None => panic!("the NMI retry delay is positive"),
    });

/// Retry advice for one submitted, determinate NMI automatic-renewal failure,
/// derived from its durable processor evidence.
///
/// Hosts return this from
/// `BillingTransactionCoordinator::renewal_decline_retry_advice` for attempts
/// whose gateway account uses [`crate::NmiPaymentGateway::provider_key`].
pub fn nmi_renewal_decline_retry_advice(evidence: &ProcessorEvidence) -> RenewalDeclineRetryAdvice {
    let Some(code) = evidence
        .response_code()
        .and_then(|code| code.expose().trim().parse::<u16>().ok())
    else {
        return RenewalDeclineRetryAdvice::FollowSchedule;
    };
    match code {
        // 220 Incorrect payment information. 221 No such card issuer.
        // 222 No card number on file with issuer. 223 Expired card.
        // 224 Invalid expiration date. 250 Pick up card. 251 Lost card.
        // 252 Stolen card.
        // 261 Declined-Stop all recurring payments.
        // 262 Declined-Stop this recurring program.
        // 263 Declined-Update cardholder data available.
        220..=224 | 250..=252 | 261..=263 => RenewalDeclineRetryAdvice::StopAutomaticRetries,
        // 264 Declined-Retry in a few days.
        264 => RenewalDeclineRetryAdvice::RetryNoEarlierThan(NMI_RETRY_IN_A_FEW_DAYS_DELAY),
        // 253 Fraudulent card is not a hard decline: NMI returns it for the
        // issuer's "suspected fraud" response (ISO 8583 code 59), which the
        // cardholder can clear with the issuer and which Visa lets a merchant
        // retry.
        // 461 Unsupported card type is a processor rejection for the merchant
        // account, like 410, 411 and 460, not an issuer decision about the
        // card.
        // Generic, funds, limit, issuer-contact and instruction-in-text
        // declines (200-204, 225, 226, 240, 260), gateway and processor
        // failures, and unpublished codes carry no retry guidance here.
        _ => RenewalDeclineRetryAdvice::FollowSchedule,
    }
}

#[cfg(test)]
mod tests {
    use syrup_rail::{GatewayDiagnostic, GatewayPaymentDescriptor};

    use super::*;

    fn evidence(code: Option<&str>) -> ProcessorEvidence {
        ProcessorEvidence::new(
            None,
            None,
            Some(GatewayDiagnostic::new("2")),
            code.map(GatewayDiagnostic::new),
            Some(GatewayDiagnostic::new("DECLINE")),
            None,
            GatewayPaymentDescriptor::default(),
        )
    }

    #[test]
    fn published_hard_declines_stop_automatic_retries() {
        for code in [
            "220", "221", "222", "223", "224", "250", "251", "252", "261", "262", "263", " 261 ",
        ] {
            assert_eq!(
                nmi_renewal_decline_retry_advice(&evidence(Some(code))),
                RenewalDeclineRetryAdvice::StopAutomaticRetries,
                "{code}"
            );
        }
    }

    #[test]
    fn retry_in_a_few_days_waits_at_least_three_days() {
        assert_eq!(
            nmi_renewal_decline_retry_advice(&evidence(Some("264"))),
            RenewalDeclineRetryAdvice::RetryNoEarlierThan(
                DunningRetryDelay::new(259_200).expect("positive delay")
            )
        );
    }

    #[test]
    fn other_or_missing_codes_follow_the_schedule() {
        for code in [
            Some("200"),
            Some("201"),
            Some("202"),
            Some("203"),
            Some("204"),
            Some("225"),
            Some("226"),
            Some("240"),
            Some("253"),
            Some("260"),
            Some("265"),
            Some("300"),
            Some("410"),
            Some("411"),
            Some("460"),
            Some("461"),
            Some("2610"),
            Some("26l"),
            Some(""),
            None,
        ] {
            assert_eq!(
                nmi_renewal_decline_retry_advice(&evidence(code)),
                RenewalDeclineRetryAdvice::FollowSchedule,
                "{code:?}"
            );
        }
    }
}
