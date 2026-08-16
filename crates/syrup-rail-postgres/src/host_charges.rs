use std::{error::Error, fmt};

use async_trait::async_trait;
use sqlx::{PgConnection, Postgres, Transaction};
use syrup_rail::{
    BillingContactSnapshot, BillingScopeId, ChargeAmount, ChargeHostTarget, HostChargeReservation,
    HostChargeTargetId, HostChargeTargetRejection, HostChargeTargetSnapshot,
    HostChargeTargetTransition, HostChargeTargetTransitionOutcome, IdempotencyKey, PaymentAttempt,
    PaymentAttemptFingerprint, PaymentAttemptId, PaymentAttemptKind, SubscriberId,
};
use thiserror::Error;

use crate::host_error::{BoxError, RedactedHostErrorSource};

/// Value-redacted failure returned by the host charge-target store.
#[derive(Debug)]
pub struct HostChargeTargetError {
    source: RedactedHostErrorSource,
}

impl HostChargeTargetError {
    /// Wraps a host error without exposing its value through ordinary error
    /// formatting or the standard error-source chain.
    pub fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: RedactedHostErrorSource::new(source),
        }
    }

    /// Returns the host error for explicit application-level inspection.
    pub fn into_source(self) -> BoxError {
        self.source.into_inner()
    }
}

impl fmt::Display for HostChargeTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("host charge target operation failed")
    }
}

impl Error for HostChargeTargetError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargeTargetReservation {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
    idempotency_key: IdempotencyKey,
}

impl HostChargeTargetReservation {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
            idempotency_key,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(&self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeReservationDecision {
    Reserved(HostChargeTargetSnapshot),
    IdempotentContender,
    Rejected {
        reason: syrup_rail::HostChargeTargetRejection,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargeSubmissionAdmission {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
    attempt_id: PaymentAttemptId,
    expected_charge: ChargeAmount,
}

impl HostChargeSubmissionAdmission {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
        attempt_id: PaymentAttemptId,
        expected_charge: ChargeAmount,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
            attempt_id,
            expected_charge,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(&self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn attempt_id(&self) -> PaymentAttemptId {
        self.attempt_id
    }

    pub const fn expected_charge(&self) -> ChargeAmount {
        self.expected_charge
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeSubmissionDecision {
    Admitted(HostChargeTargetSnapshot),
    Rejected {
        reason: syrup_rail::HostChargeTargetRejection,
    },
}

/// Host-owned target extension composed into shared ledger transactions.
///
/// Implementations must use only the supplied connection. Reservation and
/// submission lock the target before calling [`host_charge_ledger_admission`].
#[async_trait]
pub trait HostChargeTargetStore: Send + Sync {
    /// Locks and snapshots the target for replay/conflict preflight without
    /// changing host business state.
    async fn preflight_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError>;

    async fn reserve_target(
        &self,
        connection: &mut PgConnection,
        reservation: &HostChargeTargetReservation,
    ) -> Result<HostChargeReservationDecision, HostChargeTargetError>;

    async fn admit_submission(
        &self,
        connection: &mut PgConnection,
        admission: &HostChargeSubmissionAdmission,
    ) -> Result<HostChargeSubmissionDecision, HostChargeTargetError>;

    async fn apply_transition(
        &self,
        connection: &mut PgConnection,
        transition: HostChargeTargetTransition,
    ) -> Result<HostChargeTargetTransitionOutcome, HostChargeTargetError>;
}

#[derive(Debug, Error)]
pub enum HostChargeStoreError {
    #[error("host charge storage operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("host charge payment-attempt operation failed")]
    Attempt(#[from] crate::attempts::PaymentAttemptStoreError),
    #[error(transparent)]
    Target(#[from] HostChargeTargetError),
    #[error("canonical host charge state is invalid")]
    InvalidState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChargePreflightOutcome {
    Continue(HostChargeTargetSnapshot),
    Replay(Box<PaymentAttempt>),
    IdempotencyConflict,
    Rejected { reason: HostChargeTargetRejection },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChargeReservationOutcome {
    Reserved(PaymentAttempt),
    Replay(PaymentAttempt),
    IdempotencyConflict,
    Rejected { reason: HostChargeTargetRejection },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChargeSubmissionOutcome {
    Admitted(PaymentAttempt),
    AlreadyAdmitted(PaymentAttempt),
    Rejected {
        attempt: PaymentAttempt,
        reason: HostChargeTargetRejection,
    },
}

pub async fn preflight_host_charge_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    targets: &dyn HostChargeTargetStore,
    command: &ChargeHostTarget,
) -> Result<HostChargePreflightOutcome, HostChargeStoreError> {
    crate::attempts::set_enrollment_timeouts(transaction).await?;
    let target_reservation = HostChargeTargetReservation::new(
        command.billing_scope_id(),
        command.subscriber_id(),
        command.target_id(),
        command.idempotency_key().clone(),
    );
    let decision = targets
        .preflight_target(transaction, &target_reservation)
        .await?;
    let existing = crate::lock_payment_attempt_by_idempotency_in_transaction(
        transaction,
        command.billing_scope_id(),
        command.subscriber_id(),
        command.idempotency_key(),
    )
    .await?;
    let snapshot = match decision {
        HostChargeReservationDecision::Reserved(snapshot) => snapshot,
        HostChargeReservationDecision::IdempotentContender => {
            let Some(existing) = existing else {
                return Err(HostChargeStoreError::InvalidState);
            };
            return Ok(
                if host_charge_attempt_matches_command(&existing, command, None) {
                    HostChargePreflightOutcome::Replay(Box::new(existing))
                } else {
                    HostChargePreflightOutcome::IdempotencyConflict
                },
            );
        }
        HostChargeReservationDecision::Rejected { reason } => {
            return Ok(match existing {
                Some(existing) if host_charge_attempt_matches_command(&existing, command, None) => {
                    HostChargePreflightOutcome::Replay(Box::new(existing))
                }
                Some(_) => HostChargePreflightOutcome::IdempotencyConflict,
                None => HostChargePreflightOutcome::Rejected { reason },
            });
        }
    };
    let Some(existing) = existing else {
        return Ok(HostChargePreflightOutcome::Continue(snapshot));
    };
    Ok(
        if host_charge_attempt_matches_command(&existing, command, Some(snapshot)) {
            HostChargePreflightOutcome::Replay(Box::new(existing))
        } else {
            HostChargePreflightOutcome::IdempotencyConflict
        },
    )
}

pub async fn reserve_host_charge_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
) -> Result<HostChargeReservationOutcome, HostChargeStoreError> {
    crate::attempts::set_enrollment_timeouts(transaction).await?;
    let identity = reservation.identity();
    let request = reservation.request();
    let target_id = request
        .target()
        .host_charge_target_id()
        .ok_or(HostChargeStoreError::InvalidState)?;
    let target_reservation = HostChargeTargetReservation::new(
        identity.billing_scope_id(),
        identity.subscriber_id(),
        target_id,
        request.idempotency_key().clone(),
    );
    let decision = targets
        .reserve_target(transaction, &target_reservation)
        .await?;
    let snapshot = match decision {
        HostChargeReservationDecision::Reserved(snapshot) => snapshot,
        HostChargeReservationDecision::IdempotentContender => {
            let existing = crate::lock_payment_attempt_by_idempotency_in_transaction(
                transaction,
                identity.billing_scope_id(),
                identity.subscriber_id(),
                request.idempotency_key(),
            )
            .await?
            .ok_or(HostChargeStoreError::InvalidState)?;
            return Ok(
                if host_charge_attempt_matches_reservation(&existing, reservation) {
                    HostChargeReservationOutcome::Replay(existing)
                } else {
                    HostChargeReservationOutcome::IdempotencyConflict
                },
            );
        }
        HostChargeReservationDecision::Rejected { reason } => {
            return Ok(HostChargeReservationOutcome::Rejected { reason });
        }
    };
    if snapshot != reservation.snapshot() {
        return Ok(HostChargeReservationOutcome::Rejected {
            reason: HostChargeTargetRejection::ChargeChanged,
        });
    }

    let inserted = sqlx::query(
        r#"
        INSERT INTO billing_payment_attempts (
            id, billing_scope_id, subscriber_id, host_charge_target_id,
            attempt_kind, status, idempotency_key, request_fingerprint,
            amount_cents, currency, gateway_account_id,
            gateway_configuration_id, gateway_order_id,
            billing_first_name, billing_last_name, billing_email
        ) VALUES (
            $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
            $7, $8, $9, $10, $11, $12, $13, $14
        )
        ON CONFLICT (billing_scope_id, subscriber_id, idempotency_key) DO NOTHING
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(target_id.as_uuid())
    .bind(request.idempotency_key().expose())
    .bind(request.fingerprint().expose())
    .bind(request.amount().cents())
    .bind(request.amount().currency().as_str())
    .bind(identity.gateway_account_id().as_uuid())
    .bind(identity.gateway_configuration_id().as_uuid())
    .bind(request.gateway_order_id().expose())
    .bind(request.billing_contact().first_name())
    .bind(request.billing_contact().last_name())
    .bind(request.billing_contact().email())
    .execute(&mut **transaction)
    .await?;
    let attempt_id = if inserted.rows_affected() == 1 {
        identity.attempt_id()
    } else {
        crate::lock_payment_attempt_by_idempotency_in_transaction(
            transaction,
            identity.billing_scope_id(),
            identity.subscriber_id(),
            request.idempotency_key(),
        )
        .await?
        .ok_or(HostChargeStoreError::InvalidState)?
        .identity()
        .attempt_id()
    };
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        attempt_id,
    )
    .await?
    .ok_or(HostChargeStoreError::InvalidState)?;
    if !host_charge_attempt_matches_reservation(&attempt, reservation) {
        return Ok(HostChargeReservationOutcome::IdempotencyConflict);
    }
    Ok(if inserted.rows_affected() == 1 {
        HostChargeReservationOutcome::Reserved(attempt)
    } else {
        HostChargeReservationOutcome::Replay(attempt)
    })
}

pub async fn admit_host_charge_submission_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    targets: &dyn HostChargeTargetStore,
    reservation: &HostChargeReservation,
) -> Result<HostChargeSubmissionOutcome, HostChargeStoreError> {
    crate::attempts::set_enrollment_timeouts(transaction).await?;
    let identity = reservation.identity();
    let target_id = reservation.snapshot().target_id();
    let admission = HostChargeSubmissionAdmission::new(
        identity.billing_scope_id(),
        identity.subscriber_id(),
        target_id,
        identity.attempt_id(),
        reservation.snapshot().charge(),
    );
    let decision = targets.admit_submission(transaction, &admission).await?;
    let rejection = match decision {
        HostChargeSubmissionDecision::Admitted(snapshot) if snapshot == reservation.snapshot() => {
            None
        }
        HostChargeSubmissionDecision::Admitted(_) => Some(HostChargeTargetRejection::ChargeChanged),
        HostChargeSubmissionDecision::Rejected { reason } => Some(reason),
    };
    if let Some(reason) = rejection {
        let updated = sqlx::query(
            r#"
            UPDATE billing_payment_attempts
            SET status = 'failed',
                gateway_response_text = 'Host target changed before payment submission.',
                gateway_condition = 'failed', resolved_at = clock_timestamp(),
                updated_at = clock_timestamp()
            WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
                AND host_charge_target_id = $4 AND attempt_kind = 'host_charge'
                AND status = 'pending' AND submitted_at IS NULL
            "#,
        )
        .bind(identity.attempt_id().as_uuid())
        .bind(identity.billing_scope_id().as_uuid())
        .bind(identity.subscriber_id().as_uuid())
        .bind(target_id.as_uuid())
        .execute(&mut **transaction)
        .await?;
        let attempt = crate::find_payment_attempt_by_id_in_transaction(
            transaction,
            identity.billing_scope_id(),
            identity.attempt_id(),
        )
        .await?
        .ok_or(HostChargeStoreError::InvalidState)?;
        return Ok(if updated.rows_affected() == 1 {
            HostChargeSubmissionOutcome::Rejected { attempt, reason }
        } else {
            HostChargeSubmissionOutcome::AlreadyAdmitted(attempt)
        });
    }
    let updated = sqlx::query(
        r#"
        UPDATE billing_payment_attempts
        SET submitted_at = clock_timestamp(), updated_at = clock_timestamp()
        WHERE id = $1 AND billing_scope_id = $2 AND subscriber_id = $3
            AND host_charge_target_id = $4 AND attempt_kind = 'host_charge'
            AND status = 'pending' AND submitted_at IS NULL
        "#,
    )
    .bind(identity.attempt_id().as_uuid())
    .bind(identity.billing_scope_id().as_uuid())
    .bind(identity.subscriber_id().as_uuid())
    .bind(target_id.as_uuid())
    .execute(&mut **transaction)
    .await?;
    let attempt = crate::find_payment_attempt_by_id_in_transaction(
        transaction,
        identity.billing_scope_id(),
        identity.attempt_id(),
    )
    .await?
    .ok_or(HostChargeStoreError::InvalidState)?;
    Ok(if updated.rows_affected() == 1 {
        HostChargeSubmissionOutcome::Admitted(attempt)
    } else {
        HostChargeSubmissionOutcome::AlreadyAdmitted(attempt)
    })
}

fn host_charge_attempt_matches_command(
    attempt: &PaymentAttempt,
    command: &ChargeHostTarget,
    snapshot: Option<HostChargeTargetSnapshot>,
) -> bool {
    let identity = attempt.identity();
    let request = attempt.request();
    let canonical_fingerprint =
        PaymentAttemptFingerprint::for_host_charge(command.target_id(), request.amount());
    let billing_contact = command
        .billing_contact()
        .map(BillingContactSnapshot::from_billing_contact)
        .unwrap_or_else(|| BillingContactSnapshot::new(None, None));
    attempt.kind() == PaymentAttemptKind::HostCharge
        && identity.billing_scope_id() == command.billing_scope_id()
        && identity.subscriber_id() == command.subscriber_id()
        && identity.gateway_configuration_id() == command.gateway_configuration_id()
        && request.target().host_charge_target_id() == Some(command.target_id())
        && request.fingerprint() == &canonical_fingerprint
        && request.billing_contact() == &billing_contact
        && snapshot.is_none_or(|snapshot| {
            request.amount() == snapshot.charge().money()
                && request.fingerprint()
                    == &PaymentAttemptFingerprint::for_host_charge(
                        command.target_id(),
                        snapshot.charge().money(),
                    )
        })
}

fn host_charge_attempt_matches_reservation(
    attempt: &PaymentAttempt,
    reservation: &HostChargeReservation,
) -> bool {
    let identity = attempt.identity();
    let requested_identity = reservation.identity();
    let request = attempt.request();
    let requested = reservation.request();
    attempt.kind() == PaymentAttemptKind::HostCharge
        && identity.billing_scope_id() == requested_identity.billing_scope_id()
        && identity.subscriber_id() == requested_identity.subscriber_id()
        && identity.gateway_account_id() == requested_identity.gateway_account_id()
        && identity.gateway_configuration_id() == requested_identity.gateway_configuration_id()
        && request.target() == requested.target()
        && request.idempotency_key() == requested.idempotency_key()
        && request.fingerprint() == requested.fingerprint()
        && request.amount() == requested.amount()
        && request.billing_contact() == requested.billing_contact()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostChargeLedgerAdmissionMode {
    Reserve { idempotency_key: IdempotencyKey },
    Submit { attempt_id: PaymentAttemptId },
    Release,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostChargeLedgerAdmissionQuery {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    target_id: HostChargeTargetId,
    mode: HostChargeLedgerAdmissionMode,
}

impl HostChargeLedgerAdmissionQuery {
    pub const fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        target_id: HostChargeTargetId,
        mode: HostChargeLedgerAdmissionMode,
    ) -> Self {
        Self {
            billing_scope_id,
            subscriber_id,
            target_id,
            mode,
        }
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn target_id(&self) -> HostChargeTargetId {
        self.target_id
    }

    pub const fn mode(&self) -> &HostChargeLedgerAdmissionMode {
        &self.mode
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostChargeLedgerAdmission {
    Safe,
    IdempotentContender,
    Unsafe,
}

#[derive(Debug, Error)]
pub enum HostChargeLedgerAdmissionError {
    #[error("host charge ledger admission query failed")]
    Sql(#[from] sqlx::Error),
    #[error("host charge ledger admission returned an invalid result")]
    InvalidResult,
}

pub async fn host_charge_ledger_admission(
    connection: &mut PgConnection,
    query: &HostChargeLedgerAdmissionQuery,
) -> Result<HostChargeLedgerAdmission, HostChargeLedgerAdmissionError> {
    let (mode, idempotency_key, attempt_id) = match query.mode() {
        HostChargeLedgerAdmissionMode::Reserve { idempotency_key } => {
            ("reserve", Some(idempotency_key.expose()), None)
        }
        HostChargeLedgerAdmissionMode::Submit { attempt_id } => {
            ("submit", None, Some(attempt_id.into_uuid()))
        }
        HostChargeLedgerAdmissionMode::Release => ("release", None, None),
    };
    let result: String = sqlx::query_scalar(
        r#"
        SELECT billing_host_charge_ledger_admission($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(query.billing_scope_id().into_uuid())
    .bind(query.subscriber_id().into_uuid())
    .bind(query.target_id().into_uuid())
    .bind(mode)
    .bind(idempotency_key)
    .bind(attempt_id)
    .fetch_one(connection)
    .await?;

    match result.as_str() {
        "safe" => Ok(HostChargeLedgerAdmission::Safe),
        "idempotent_contender" => Ok(HostChargeLedgerAdmission::IdempotentContender),
        "unsafe" => Ok(HostChargeLedgerAdmission::Unsafe),
        _ => Err(HostChargeLedgerAdmissionError::InvalidResult),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use uuid::Uuid;

    use super::*;
    use crate::test_support::{TestDatabase, create_gateway_account};

    #[tokio::test]
    async fn admission_distinguishes_safe_contender_and_unsafe_modes() -> Result<(), Box<dyn Error>>
    {
        let database = TestDatabase::start("rail_host_admit").await?;
        let result = async {
            let gateway = create_gateway_account(&database.pool, "test_gateway").await?;
            let subscriber_id = SubscriberId::new(Uuid::now_v7());
            let target_id = HostChargeTargetId::new(Uuid::now_v7());
            let idempotency_key = IdempotencyKey::new("host-charge-test")?;

            let mut connection = database.pool.acquire().await?;
            let reserve = HostChargeLedgerAdmissionQuery::new(
                BillingScopeId::new(gateway.billing_scope_id),
                subscriber_id,
                target_id,
                HostChargeLedgerAdmissionMode::Reserve {
                    idempotency_key: idempotency_key.clone(),
                },
            );
            assert_eq!(
                host_charge_ledger_admission(&mut connection, &reserve).await?,
                HostChargeLedgerAdmission::Safe
            );

            let attempt_id = PaymentAttemptId::new(Uuid::now_v7());
            sqlx::query(
                r#"
                INSERT INTO billing_payment_attempts (
                    id, billing_scope_id, subscriber_id, host_charge_target_id,
                    attempt_kind, status, idempotency_key, request_fingerprint,
                    amount_cents, currency, gateway_account_id,
                    gateway_configuration_id, gateway_order_id
                ) VALUES (
                    $1, $2, $3, $4, 'host_charge', 'pending', $5, $6,
                    100, 'USD', $7, $8, 'host-charge-test-order'
                )
                "#,
            )
            .bind(attempt_id.as_uuid())
            .bind(gateway.billing_scope_id)
            .bind(subscriber_id.as_uuid())
            .bind(target_id.as_uuid())
            .bind(idempotency_key.expose())
            .bind(format!("host_charge:{}:100:USD", target_id.as_uuid()))
            .bind(gateway.gateway_account_id)
            .bind(gateway.gateway_configuration_id)
            .execute(&mut *connection)
            .await?;

            assert_eq!(
                host_charge_ledger_admission(&mut connection, &reserve).await?,
                HostChargeLedgerAdmission::IdempotentContender
            );
            let submit = HostChargeLedgerAdmissionQuery::new(
                BillingScopeId::new(gateway.billing_scope_id),
                subscriber_id,
                target_id,
                HostChargeLedgerAdmissionMode::Submit { attempt_id },
            );
            assert_eq!(
                host_charge_ledger_admission(&mut connection, &submit).await?,
                HostChargeLedgerAdmission::Safe
            );
            let release = HostChargeLedgerAdmissionQuery::new(
                BillingScopeId::new(gateway.billing_scope_id),
                subscriber_id,
                target_id,
                HostChargeLedgerAdmissionMode::Release,
            );
            assert_eq!(
                host_charge_ledger_admission(&mut connection, &release).await?,
                HostChargeLedgerAdmission::Unsafe
            );
            Ok::<_, Box<dyn Error>>(())
        }
        .await;
        let cleanup = database.cleanup().await;
        result?;
        cleanup
    }
}
