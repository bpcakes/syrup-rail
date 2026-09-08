//! Application-independent subscription billing domain types and policy.
//!
//! PostgreSQL orchestration lives in `syrup-rail-postgres`; NMI adaptation lives
//! in `syrup-rail-nmi` and `syrup-rail-nmi-client`.

#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod admission;
mod approval_evidence;
mod attempt;
mod audit_reason;
mod billing_portal;
mod card_data;
mod discount;
mod enrollment;
mod event;
mod gateway;
mod gateway_value;
mod host_charge;
mod identity;
mod legacy_gateway_policy;
mod money;
mod operator_review;
mod payment_method_update;
mod policy;
mod reconciliation;
mod recovery;
mod renewal;
mod resolution;
mod resolver;
mod subscription;
mod subscription_payment_context;
mod terms;

pub use approval_evidence::{ProcessorApprovalEvidence, ProcessorApprovalEvidenceParseError};
#[allow(deprecated)]
pub use legacy_gateway_policy::{gateway_response_is_approved, gateway_state_is_approved};

pub use admission::{
    EndUserMutationAdmission, EndUserMutationAdmissionResult, EndUserMutationCommand,
    EndUserMutationOperation, EndUserMutationRetryAfter, EndUserMutationRetryAfterError,
};
pub use attempt::{
    BillingContactSnapshot, PaymentAttempt, PaymentAttemptError, PaymentAttemptFingerprint,
    PaymentAttemptFingerprintError, PaymentAttemptIdentity, PaymentAttemptLifecycle,
    PaymentAttemptRequest, PaymentAttemptSnapshotError, PaymentAttemptState, PaymentAttemptTarget,
    PaymentAttemptTimestamps, PaymentMethodUpdateSnapshot, SubscriptionEnrollmentTermsVersion,
    SubscriptionInitialApplication, SubscriptionPaymentStateSnapshot,
};
pub use billing_portal::{
    SUBSCRIPTION_PAYMENT_HISTORY_PAGE_LIMIT, SubscriptionBillingPortalQuery,
    SubscriptionBillingPortalSnapshot, SubscriptionPaymentHistoryCursor,
    SubscriptionPaymentHistoryItem, SubscriptionPaymentHistoryItemError,
    SubscriptionPaymentHistoryPage, SubscriptionPaymentHistoryPageLimit,
    SubscriptionPaymentHistoryPageLimitError, SubscriptionPaymentMethodDisplay,
    SubscriptionPaymentMethodDisplayError,
};
pub use card_data::{raw_card_data_ranges, string_contains_raw_card_data};
pub use discount::{
    ClearSubscriptionDiscount, SubscriptionDiscountClaim, SubscriptionDiscountClaimOutcome,
    SubscriptionDiscountClaimRecord, SubscriptionDiscountClaimState,
    SubscriptionDiscountClaimStatus, SubscriptionDiscountClearOutcome,
    SubscriptionDiscountCodeCreation, SubscriptionDiscountCodeQuote,
    SubscriptionDiscountCodeRecord, SubscriptionDiscountCodeStatus, SubscriptionDiscountCodeUpdate,
    discounted_charge,
};
pub use enrollment::{
    EnrollSubscription, SubscriptionActivationProjection, SubscriptionEnrollmentDiscountSnapshot,
    SubscriptionEnrollmentExpectedTerms, SubscriptionEnrollmentPaymentResult,
    SubscriptionEnrollmentPaymentResultBuildError, SubscriptionEnrollmentPreflightOutcome,
    SubscriptionEnrollmentReservation, SubscriptionEnrollmentReservationBuildError,
    SubscriptionEnrollmentReservationOutcome, SubscriptionEnrollmentReservationRejection,
    SubscriptionEnrollmentSubmissionOutcome, SubscriptionEnrollmentSubmissionRejection,
    SubscriptionEnrollmentTermsError,
};
pub use event::{
    BillingEvent, BillingEventKey, BillingEventSubject, PaymentCardDisplay, SubscriptionEndReason,
    SubscriptionPaymentFailureAccess, SubscriptionPaymentFailureDisposition,
    SubscriptionPaymentFailureOutcome,
};
pub use gateway::{
    ApprovedProcessorEvidence, CardLastFour, GATEWAY_MUTATION_RATE_LIMIT_RETRY_AFTER_SECONDS,
    GatewayAccountMode, GatewayAccountModeParseError, GatewayError, GatewayLifecycleEvidence,
    GatewayLifecycleEvidenceError, GatewayLifecycleQuarantine, GatewayLifecycleQuarantineError,
    GatewayLifecycleQuarantineReason, GatewayLifecycleQuarantineResolutionReason,
    GatewayLifecycleQuarantineResolutionReasonError, GatewayLifecycleQueryPolicy,
    GatewayLifecycleQueryPolicyError, GatewayLifecycleState, GatewayMutationError,
    GatewayMutationReferenceFactory, GatewayNotSubmittedError, GatewayPaymentDescriptor,
    GatewayPaymentDiagnostic, GatewayPaymentMethodMetadata, GatewayPaymentOutcome,
    GatewayPaymentStatus, GatewayQueryRequest, GatewayRequestError, GatewaySaleIntent,
    GatewaySaleRequest, GatewayStorePaymentMethodRequest, GatewayTransactionReport,
    GatewayTransactionReportRequest, MutationCertainty, PaymentCardBrand, PaymentGateway,
    PaymentReversalKind, ProcessorEvidence, SharedGatewayMutationReferenceFactory,
    SharedPaymentGateway,
};
pub use gateway_value::{
    BillingContact, BillingContactError, GatewayDiagnostic, GatewayOrderId,
    GatewayPaymentMethodReference, GatewayReferenceValueError, GatewayTransactionId,
    MAX_BILLING_CONTACT_FIELD_BYTES, MAX_GATEWAY_TEXT_BYTES, PaymentToken, PaymentTokenError,
    canonical_gateway_transaction_id, canonical_gateway_transaction_ids_equal,
    sanitize_gateway_detail, truncate_gateway_detail_to_length,
};
pub use host_charge::{
    ChargeHostTarget, HostChargePaymentResult, HostChargePaymentResultBuildError,
    HostChargeReservation, HostChargeReservationBuildError, HostChargeTargetNoChange,
    HostChargeTargetRejection, HostChargeTargetSnapshot, HostChargeTargetTransition,
    HostChargeTargetTransitionKind, HostChargeTargetTransitionOutcome,
};
pub use identity::{
    ActorId, BillingScopeId, DiscountClaimId, DiscountCodeId, GatewayAccountId,
    GatewayAccountIdentity, GatewayAccountRegistration, GatewayConfigurationActivation,
    GatewayConfigurationActivationOutcome, GatewayConfigurationId, GatewayLifecycleCursorKey,
    GatewayProviderKey, HostChargeTargetId, IdempotencyKey, IdempotencyKeyError, PaymentAttemptId,
    PaymentAttemptKind, PaymentAttemptKindParseError, PaymentAttemptStatus,
    PaymentAttemptStatusParseError, PaymentMethodId, PaymentMethodStatus, PlanKey,
    ProcessorChargeId, SlugError, SubscriberId, SubscriptionGrantId, SubscriptionId,
    SubscriptionPhase, SubscriptionPhaseParseError, SubscriptionStatus,
    SubscriptionStatusParseError,
};
pub use money::{
    BillingPeriod, BillingPeriodError, ChargeAmount, CumulativeRefundCents, CurrencyCode,
    CurrencyCodeError, Money, MoneyError,
};
pub use operator_review::{
    AttemptReviewCursor, AttemptReviewPage, ExternalReversalAttestation,
    ExternalReversalHostChargeRelease, ExternalReversalKind, ExternalReversalOutcome,
    ExternalReversalPriorClassification, ExternalReversalReason, ExternalReversalReasonError,
    ExternalReversalResolution, ExternalReversalResolutionError, MANUAL_ATTEMPT_FAILURE_NOTE,
    ManualAttemptFailureOutcome, ManualFailureHostCharge, OPERATOR_REVIEW_PAGE_LIMIT,
    OperatorReviewPageLimit, OperatorReviewPageLimitError,
    PAYMENT_METHOD_UPDATE_MANUAL_CLOSURE_NOTE, ProcessorCharge, ProcessorChargeProgression,
    ProcessorChargeReviewCursor, ProcessorChargeReviewItem, ProcessorChargeReviewPage,
    ProcessorChargeRole, ProcessorChargeStateCode, review_required_attempt_can_be_manually_failed,
    review_required_manual_failure_evidence,
};
pub use payment_method_update::{
    ReplaceSubscriptionPaymentMethod, SubscriptionPaymentMethodReplacement,
    SubscriptionPaymentMethodReplacementBuildError,
    SubscriptionPaymentMethodReplacementLockedTerms,
    SubscriptionPaymentMethodReplacementPreflightOutcome,
    SubscriptionPaymentMethodReplacementRejection,
    SubscriptionPaymentMethodReplacementReservationOutcome,
    SubscriptionPaymentMethodReplacementSubmissionOutcome,
    SubscriptionPaymentMethodReplacementSubmissionRejection,
};
pub use policy::{BillingPeriodPolicyError, next_billing_period};
pub use reconciliation::{GatewayAccountReconciliationCandidate, GatewayLifecycleAccount};
pub use recovery::{
    RecoverSubscriptionPayment, SubscriptionRecoveryLockedTerms,
    SubscriptionRecoveryPreflightOutcome, SubscriptionRecoveryReservation,
    SubscriptionRecoveryReservationBuildError, SubscriptionRecoveryReservationOutcome,
    SubscriptionRecoveryReservationRejection, SubscriptionRecoverySubmissionOutcome,
    SubscriptionRecoverySubmissionRejection,
};
pub use renewal::{
    ChargeRenewal, MAX_RENEWAL_INFRASTRUCTURE_ATTEMPTS_PER_PERIOD_CONFIGURATION,
    RENEWAL_DISPATCH_LIMIT, RENEWAL_INFRASTRUCTURE_RETRY_AFTER_SECONDS,
    RENEWAL_RATE_LIMIT_FAST_RETRY_ATTEMPTS, RENEWAL_RATE_LIMIT_SLOW_RETRY_AFTER_SECONDS,
    RenewalAttemptState, RenewalDispatch, RenewalDispatchPage, RenewalDispatchPageCursor,
    RenewalFailureDisposition, RenewalFailurePolicyError, SubscriptionRenewalLockedTerms,
    SubscriptionRenewalOutcome, SubscriptionRenewalReservation,
    SubscriptionRenewalReservationBuildError, SubscriptionRenewalReservationOutcome,
    SubscriptionRenewalReservationRejection, SubscriptionRenewalSubmissionOutcome,
    SubscriptionRenewalSubmissionRejection, rate_limit_retry_after_seconds,
    renewal_attempt_idempotency_key, renewal_failure_disposition,
};
pub use resolution::{PaymentResolutionCode, PaymentResolutionCodeParseError};
pub use resolver::{GatewayResolutionError, GatewayResolver, ResolvedGateway};
pub use subscription::{
    AppliedSubscriptionDiscount, BillingDeletionBlockers, CancelSubscription,
    CancelSubscriptionOutcome, DeletionBlockerQuery, Entitlement, EntitlementGuard,
    EntitlementQuery, LimitedDiscountMonths, MissingSubscriptionAction, PastDueAccess,
    PastDueAction, PercentOffBasisPoints, PositiveDiscountCents, SavedSubscriptionDiscount,
    ScrubSubscriberBillingData, ScrubbedBillingRows, Subscription, SubscriptionDiscountCode,
    SubscriptionDiscountDuration, SubscriptionDiscountError, SubscriptionDiscountKind,
    SubscriptionDiscountSnapshot, SubscriptionGrant, SubscriptionGrantCreation,
    SubscriptionGrantCreationOutcome, SubscriptionGrantError, SubscriptionGrantKind,
    SubscriptionGrantKindParseError, SubscriptionGrantReason, SubscriptionGrantReasonError,
    SubscriptionGrantRecord, SubscriptionGrantRecordError, SubscriptionGrantRevocation,
    SubscriptionGrantRevocationAudit, SubscriptionGrantRevocationOutcome,
    SubscriptionGrantRevocationState, SubscriptionLifecycle, SubscriptionLifecycleError,
    classify_past_due_access,
};
pub use subscription_payment_context::SubscriptionPaymentContext;
pub use terms::{
    DunningExhaustion, DunningRetryDelay, DunningSchedule, MAX_DUNNING_RETRY_STEPS, PaidTrialTerms,
    PastDueAccessPolicy, RecurringSubscriptionTerms, RenewalFailurePolicy, SubscriptionOffer,
    SubscriptionPeriodRule, SubscriptionStart, SubscriptionTermsError, SubscriptionTermsParseError,
};

/// Workspace package version exposed for consumer pinning diagnostics.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
