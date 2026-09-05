async fn attempt_candidates(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<Vec<AttemptCandidate>, GatewayLifecycleReconciliationError> {
    let rows = sqlx::query(
        r#"
        SELECT attempts.id,
            attempts.billing_scope_id,
            attempts.subscriber_id,
            attempts.attempt_kind,
            attempts.host_charge_target_id,
            attempts.amount_cents,
            attempts.gateway_lifecycle_status,
            attempts.gateway_lifecycle_at,
            attempts.refunded_amount_cents,
            COALESCE((
                $1::text IS NOT NULL
                AND public.billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = $1
            ), false) AS matched_transaction_id,
            COALESCE((
                $2::text IS NOT NULL
                AND (
                    $1::text IS NULL
                    OR public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) IS NULL
                )
                AND attempts.gateway_order_id = $2
            ), false) AS matched_order_id
        FROM billing_payment_attempts attempts
        WHERE attempts.billing_scope_id = $3
            AND attempts.gateway_account_id = $4
            AND attempts.status = 'approved'
            AND (
                (
                    $1::text IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) = $1
                )
                OR (
                    $2::text IS NOT NULL
                    AND (
                        $1::text IS NULL
                        OR public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) IS NULL
                    )
                    AND attempts.gateway_order_id = $2
                )
            )
        ORDER BY attempts.created_at, attempts.id
        FOR UPDATE OF attempts
        "#,
    )
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .fetch_all(&mut **transaction)
    .await?;
    rows.into_iter()
        .map(|row| {
            let current_refunded_amount_cents = row.try_get("refunded_amount_cents")?;
            let current_state = lifecycle_state_from_parts(
                row.try_get::<String, _>("gateway_lifecycle_status")?
                    .as_str(),
                Some(current_refunded_amount_cents),
                true,
            )?;
            Ok(AttemptCandidate {
                id: row.try_get("id")?,
                billing_scope_id: BillingScopeId::new(row.try_get("billing_scope_id")?),
                subscriber_id: SubscriberId::new(row.try_get("subscriber_id")?),
                kind: row
                    .try_get::<String, _>("attempt_kind")?
                    .parse()
                    .map_err(|_| {
                        GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
                    })?,
                host_charge_target_id: row
                    .try_get::<Option<Uuid>, _>("host_charge_target_id")?
                    .map(HostChargeTargetId::new),
                amount_cents: row.try_get("amount_cents")?,
                current_state,
                current_lifecycle_at: row.try_get("gateway_lifecycle_at")?,
                current_refunded_amount_cents,
                matched_transaction_id: row.try_get("matched_transaction_id")?,
                matched_order_id: row.try_get("matched_order_id")?,
            })
        })
        .collect()
}

fn lifecycle_transition(
    current_state: &GatewayLifecycleState,
    current_refunded_amount_cents: i32,
    current_lifecycle_at: Option<DateTime<Utc>>,
    incoming_state: &GatewayLifecycleState,
    incoming_lifecycle_at: Option<DateTime<Utc>>,
    captured_amount_cents: i32,
) -> Result<LifecycleTransition, GatewayLifecycleReconciliationError> {
    if !stored_amount_is_valid(
        current_state,
        current_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_STORED_STATE,
        ));
    }
    let incoming_refunded_amount_cents = lifecycle_refunded_amount(incoming_state);
    if !incoming_amount_is_valid(incoming_state, captured_amount_cents) {
        return Ok(LifecycleTransition::InvalidRefundEconomics);
    }
    let incoming_rank = lifecycle_rank(incoming_state);
    let current_rank = lifecycle_rank(current_state);
    let should_advance = incoming_rank > current_rank
        || (incoming_rank == current_rank
            && (current_lifecycle_at.is_none()
                || incoming_lifecycle_at
                    .zip(current_lifecycle_at)
                    .is_some_and(|(incoming, current)| incoming > current)
                || incoming_refunded_amount_cents
                    .is_some_and(|incoming| incoming > current_refunded_amount_cents)));
    if !should_advance {
        return Ok(LifecycleTransition::AlreadySuperseded);
    }
    let next_refunded_amount_cents = current_refunded_amount_cents
        .max(incoming_refunded_amount_cents.unwrap_or(current_refunded_amount_cents));
    if !stored_amount_is_valid(
        incoming_state,
        next_refunded_amount_cents,
        captured_amount_cents,
    ) {
        return Ok(LifecycleTransition::ConflictingLifecycleEvidence);
    }
    Ok(LifecycleTransition::Apply {
        refunded_amount_cents: next_refunded_amount_cents,
    })
}

const fn lifecycle_rank(state: &GatewayLifecycleState) -> i16 {
    match state {
        GatewayLifecycleState::Unknown => 0,
        GatewayLifecycleState::PendingSettlement => 1,
        GatewayLifecycleState::Settled { .. } => 2,
        GatewayLifecycleState::Voided => 3,
        GatewayLifecycleState::Refunded { .. } => 4,
        GatewayLifecycleState::Chargeback { .. } => 5,
    }
}

const fn lifecycle_status(state: &GatewayLifecycleState) -> &'static str {
    match state {
        GatewayLifecycleState::Unknown => "unknown",
        GatewayLifecycleState::PendingSettlement => "pending_settlement",
        GatewayLifecycleState::Settled { .. } => "settled",
        GatewayLifecycleState::Voided => "voided",
        GatewayLifecycleState::Refunded { .. } => "refunded",
        GatewayLifecycleState::Chargeback { .. } => "chargeback",
    }
}

const fn lifecycle_refunded_amount(state: &GatewayLifecycleState) -> Option<i32> {
    match state {
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        }
        | GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => match cumulative_refunded_cents {
            Some(amount) => Some(amount.get()),
            None => None,
        },
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => Some(cumulative_refunded_cents.get()),
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => None,
    }
}

fn incoming_amount_is_valid(state: &GatewayLifecycleState, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => true,
        GatewayLifecycleState::Settled {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() < captured),
        GatewayLifecycleState::Refunded {
            cumulative_refunded_cents,
        } => captured > 0 && cumulative_refunded_cents.get() == captured,
        GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents,
        } => {
            captured > 0 && cumulative_refunded_cents.is_none_or(|amount| amount.get() <= captured)
        }
    }
}

fn stored_amount_is_valid(state: &GatewayLifecycleState, refunded: i32, captured: i32) -> bool {
    match state {
        GatewayLifecycleState::Unknown
        | GatewayLifecycleState::PendingSettlement
        | GatewayLifecycleState::Voided => refunded == 0,
        GatewayLifecycleState::Settled { .. } => {
            captured > 0 && refunded >= 0 && refunded < captured
        }
        GatewayLifecycleState::Refunded { .. } => captured > 0 && refunded == captured,
        GatewayLifecycleState::Chargeback { .. } => {
            captured > 0 && refunded >= 0 && refunded <= captured
        }
    }
}

fn lifecycle_state_from_parts(
    status: &str,
    refunded: Option<i32>,
    stored_attempt: bool,
) -> Result<GatewayLifecycleState, GatewayLifecycleReconciliationError> {
    let state = match (status, refunded) {
        ("unknown", None | Some(0)) => GatewayLifecycleState::Unknown,
        ("pending_settlement", None | Some(0)) => GatewayLifecycleState::PendingSettlement,
        ("voided", None | Some(0)) => GatewayLifecycleState::Voided,
        ("settled", None | Some(0)) => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: None,
        },
        ("settled", Some(value)) if value > 0 => GatewayLifecycleState::Settled {
            cumulative_refunded_cents: Some(CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?),
        },
        ("refunded", Some(value)) if value > 0 => GatewayLifecycleState::Refunded {
            cumulative_refunded_cents: CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?,
        },
        ("chargeback", None | Some(0)) => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: None,
        },
        ("chargeback", Some(value)) if value > 0 => GatewayLifecycleState::Chargeback {
            cumulative_refunded_cents: Some(CumulativeRefundCents::new(value).map_err(|_| {
                GatewayLifecycleReconciliationError::InvalidState(INVALID_STORED_STATE)
            })?),
        },
        _ => {
            return Err(GatewayLifecycleReconciliationError::InvalidState(
                INVALID_STORED_STATE,
            ));
        }
    };
    if stored_attempt && refunded.is_none() {
        return Err(GatewayLifecycleReconciliationError::InvalidState(
            INVALID_STORED_STATE,
        ));
    }
    Ok(state)
}

async fn stage_evidence(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<bool, GatewayLifecycleReconciliationError> {
    // A successful insert retains this candidate identity; the conflict update
    // retains the existing row's identity. This keeps classification exact in
    // one concurrency-safe statement without inspecting MVCC system columns.
    let candidate_id = Uuid::now_v7();
    let inserted = sqlx::query_scalar::<_, bool>(
        r#"
        INSERT INTO billing_gateway_lifecycle_pending_updates (
            billing_scope_id,
            gateway_account_id,
            gateway_transaction_id,
            gateway_order_id,
            gateway_condition,
            gateway_lifecycle_status,
            gateway_lifecycle_action,
            gateway_lifecycle_at,
            refunded_amount_cents,
            expires_at,
            id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9,
            now() + ($10::bigint * interval '1 second'), $11)
        ON CONFLICT (
            gateway_account_id,
            COALESCE(gateway_transaction_id, ''),
            COALESCE(gateway_order_id, ''),
            COALESCE(gateway_condition, ''),
            gateway_lifecycle_status,
            COALESCE(gateway_lifecycle_action, ''),
            COALESCE(gateway_lifecycle_at, '-infinity'),
            COALESCE(refunded_amount_cents, -1)
        ) DO UPDATE SET updated_at = now()
        RETURNING id = $11
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .bind(evidence.condition.as_deref())
    .bind(lifecycle_status(&evidence.state))
    .bind(evidence.action.as_deref())
    .bind(evidence.effective_at)
    .bind(lifecycle_refunded_amount(&evidence.state))
    .bind(PENDING_RETENTION_SECONDS)
    .bind(candidate_id)
    .fetch_one(&mut **transaction)
    .await?;
    Ok(inserted)
}

async fn record_quarantine(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
    quarantine: &GatewayLifecycleQuarantine,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    record_quarantine_parts(
        &mut transaction,
        account,
        quarantine
            .transaction_id()
            .map(GatewayTransactionId::expose),
        quarantine.order_id().map(GatewayOrderId::expose),
        quarantine.reason(),
    )
    .await?;
    transaction.commit().await?;
    Ok(())
}

async fn record_quarantine_parts(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    transaction_id: Option<&str>,
    order_id: Option<&str>,
    reason: GatewayLifecycleQuarantineReason,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        INSERT INTO billing_gateway_lifecycle_quarantines (
            billing_scope_id,
            gateway_account_id,
            gateway_transaction_id,
            gateway_order_id,
            reason_code
        )
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (
            gateway_account_id,
            COALESCE(gateway_transaction_id, ''),
            COALESCE(gateway_order_id, ''),
            reason_code
        ) DO UPDATE
        SET last_seen_at = now(),
            occurrence_count = billing_gateway_lifecycle_quarantines.occurrence_count + 1,
            resolved_at = NULL,
            last_operator_alerted_at = CASE
                WHEN billing_gateway_lifecycle_quarantines.resolved_at IS NULL
                    THEN billing_gateway_lifecycle_quarantines.last_operator_alerted_at
                ELSE NULL
            END
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(transaction_id)
    .bind(order_id)
    .bind(reason.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn resolve_matching_quarantines(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    evidence: &StoredEvidence,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        UPDATE billing_gateway_lifecycle_quarantines
        SET resolved_at = now()
        WHERE billing_scope_id = $1
            AND gateway_account_id = $2
            AND resolved_at IS NULL
            AND (
                (
                    $3::text IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        gateway_transaction_id
                    ) = $3
                )
                OR (
                    gateway_transaction_id IS NULL
                    AND gateway_order_id IS NOT NULL
                    AND $4::text IS NOT NULL
                    AND gateway_order_id = $4
                )
            )
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(evidence.transaction_id.as_deref())
    .bind(evidence.order_id.as_deref())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn delete_pending(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
    pending_id: Uuid,
) -> Result<(), GatewayLifecycleReconciliationError> {
    sqlx::query(
        r#"
        DELETE FROM billing_gateway_lifecycle_pending_updates
        WHERE id = $1
            AND billing_scope_id = $2
            AND gateway_account_id = $3
        "#,
    )
    .bind(pending_id)
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn cleanup_pending(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
) -> Result<u64, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let result = sqlx::query(cleanup_pending_sql())
        .bind(account.billing_scope_id().as_uuid())
        .bind(account.gateway_account_id().as_uuid())
        .bind(PENDING_CLEANUP_BATCH_SIZE)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(result.rows_affected())
}

fn cleanup_pending_sql() -> &'static str {
    r#"
    WITH stale AS MATERIALIZED (
        SELECT pending.id
        FROM billing_gateway_lifecycle_pending_updates pending
        WHERE pending.billing_scope_id = $1
            AND pending.gateway_account_id = $2
            AND pending.expires_at <= now()
        ORDER BY pending.expires_at, pending.first_seen_at, pending.id
        LIMIT $3
        FOR UPDATE SKIP LOCKED
    )
    DELETE FROM billing_gateway_lifecycle_pending_updates pending
    USING stale
    WHERE pending.id = stale.id
    "#
}

async fn actionable_pending(
    pool: &PgPool,
    account: &GatewayLifecycleAccount,
) -> Result<Vec<(Uuid, StoredEvidence)>, GatewayLifecycleReconciliationError> {
    let mut transaction = pool.begin().await?;
    set_timeouts(&mut transaction).await?;
    ensure_account(&mut transaction, account).await?;
    let rows = sqlx::query(actionable_pending_sql())
        .bind(account.billing_scope_id().as_uuid())
        .bind(account.gateway_account_id().as_uuid())
        .bind(STAGED_APPLICATION_BATCH_SIZE)
        .fetch_all(&mut *transaction)
        .await?;
    let pending = rows
        .into_iter()
        .map(|row| {
            let status: String = row.try_get("gateway_lifecycle_status")?;
            let refunded = row.try_get("refunded_amount_cents")?;
            Ok((
                row.try_get("id")?,
                StoredEvidence {
                    transaction_id: row.try_get("gateway_transaction_id")?,
                    order_id: row.try_get("gateway_order_id")?,
                    state: lifecycle_state_from_parts(&status, refunded, false)?,
                    condition: row.try_get("gateway_condition")?,
                    action: row.try_get("gateway_lifecycle_action")?,
                    effective_at: row.try_get("gateway_lifecycle_at")?,
                },
            ))
        })
        .collect::<Result<Vec<_>, GatewayLifecycleReconciliationError>>()?;
    transaction.commit().await?;
    Ok(pending)
}

fn actionable_pending_sql() -> &'static str {
    r#"
    SELECT pending.id,
        pending.gateway_transaction_id,
        pending.gateway_order_id,
        pending.gateway_condition,
        pending.gateway_lifecycle_status,
        pending.gateway_lifecycle_action,
        pending.gateway_lifecycle_at,
        pending.refunded_amount_cents
    FROM billing_gateway_lifecycle_pending_updates pending
    CROSS JOIN LATERAL (
        SELECT COUNT(*)::bigint AS candidate_count,
            COALESCE(BOOL_OR(
                pending.gateway_transaction_id IS NOT NULL
                AND public.billing_canonical_gateway_transaction_id(
                    attempts.gateway_transaction_id
                ) = pending.gateway_transaction_id
            ), false) AS has_transaction_match
        FROM billing_payment_attempts attempts
        WHERE attempts.billing_scope_id = $1
            AND attempts.gateway_account_id = $2
            AND attempts.status = 'approved'
            AND (
                (
                    pending.gateway_transaction_id IS NOT NULL
                    AND public.billing_canonical_gateway_transaction_id(
                        attempts.gateway_transaction_id
                    ) = pending.gateway_transaction_id
                )
                OR (
                    pending.gateway_order_id IS NOT NULL
                    AND (
                        pending.gateway_transaction_id IS NULL
                        OR public.billing_canonical_gateway_transaction_id(
                            attempts.gateway_transaction_id
                        ) IS NULL
                    )
                    AND attempts.gateway_order_id = pending.gateway_order_id
                )
            )
    ) matches
    WHERE pending.billing_scope_id = $1
        AND pending.gateway_account_id = $2
        AND pending.expires_at > now()
        AND (matches.has_transaction_match OR matches.candidate_count = 1)
    ORDER BY pending.first_seen_at, pending.id
    LIMIT $3
    FOR UPDATE OF pending SKIP LOCKED
    "#
}

pub(crate) async fn ensure_account(
    transaction: &mut Transaction<'_, Postgres>,
    account: &GatewayLifecycleAccount,
) -> Result<(), GatewayLifecycleReconciliationError> {
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM billing_gateway_accounts
            WHERE billing_scope_id = $1
                AND id = $2
                AND provider_key = $3
            FOR KEY SHARE
        )
        "#,
    )
    .bind(account.billing_scope_id().as_uuid())
    .bind(account.gateway_account_id().as_uuid())
    .bind(account.provider_key().as_str())
    .fetch_one(&mut **transaction)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(GatewayLifecycleReconciliationError::AccountNotFound)
    }
}

pub(crate) async fn set_timeouts(
    transaction: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    crate::transaction_support::set_local_timeouts(transaction, ROW_LOCK_TIMEOUT, OPERATION_TIMEOUT)
        .await
}

fn latest_time(left: Option<DateTime<Utc>>, right: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}
