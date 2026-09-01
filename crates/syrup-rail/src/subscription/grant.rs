use super::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionGrantKind {
    Testing,
    Promotion,
}

impl SubscriptionGrantKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Testing => "testing",
            Self::Promotion => "promotion",
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("unknown subscription grant kind")]
pub struct SubscriptionGrantKindParseError;

impl FromStr for SubscriptionGrantKind {
    type Err = SubscriptionGrantKindParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "testing" => Ok(Self::Testing),
            "promotion" => Ok(Self::Promotion),
            _ => Err(SubscriptionGrantKindParseError),
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantError {
    #[error("subscription grant end must be after its start")]
    InvalidPeriod,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrant {
    id: SubscriptionGrantId,
    plan_key: PlanKey,
    kind: SubscriptionGrantKind,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    granted_by_actor_id: ActorId,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantReasonError {
    #[error("subscription grant reason is empty")]
    Empty,
    #[error("subscription grant reason exceeds 500 characters")]
    TooLong,
    #[error("subscription grant reason contains raw payment card data")]
    ContainsRawCardData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantReason(String);

impl SubscriptionGrantReason {
    pub fn new(value: impl Into<String>) -> Result<Self, SubscriptionGrantReasonError> {
        let value = value.into();
        let value = value.trim();
        let length = value.chars().count();
        if length == 0 {
            return Err(SubscriptionGrantReasonError::Empty);
        }
        if length > 500 {
            return Err(SubscriptionGrantReasonError::TooLong);
        }
        if crate::string_contains_raw_card_data(value) {
            return Err(SubscriptionGrantReasonError::ContainsRawCardData);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionGrantRecordError {
    #[error("subscription grant revocation fields must be all present or all absent")]
    InvalidRevocation,
    #[error("subscription grant revocation cannot precede the grant start")]
    RevocationBeforeStart,
    #[error("subscription grant update cannot precede creation")]
    UpdateBeforeCreation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantRecord {
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    grant: SubscriptionGrant,
    reason: SubscriptionGrantReason,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by_actor_id: Option<ActorId>,
    revocation_reason: Option<SubscriptionGrantReason>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl SubscriptionGrantRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        grant: SubscriptionGrant,
        reason: SubscriptionGrantReason,
        revoked_at: Option<DateTime<Utc>>,
        revoked_by_actor_id: Option<ActorId>,
        revocation_reason: Option<SubscriptionGrantReason>,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, SubscriptionGrantRecordError> {
        if !matches!(
            (&revoked_at, &revoked_by_actor_id, &revocation_reason),
            (None, None, None) | (Some(_), Some(_), Some(_))
        ) {
            return Err(SubscriptionGrantRecordError::InvalidRevocation);
        }
        if revoked_at.is_some_and(|revoked_at| revoked_at < *grant.starts_at()) {
            return Err(SubscriptionGrantRecordError::RevocationBeforeStart);
        }
        if updated_at < created_at {
            return Err(SubscriptionGrantRecordError::UpdateBeforeCreation);
        }
        Ok(Self {
            billing_scope_id,
            subscriber_id,
            grant,
            reason,
            revoked_at,
            revoked_by_actor_id,
            revocation_reason,
            created_at,
            updated_at,
        })
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn grant(&self) -> &SubscriptionGrant {
        &self.grant
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }

    pub const fn revoked_at(&self) -> Option<&DateTime<Utc>> {
        self.revoked_at.as_ref()
    }

    pub const fn revoked_by_actor_id(&self) -> Option<ActorId> {
        self.revoked_by_actor_id
    }

    pub const fn revocation_reason(&self) -> Option<&SubscriptionGrantReason> {
        self.revocation_reason.as_ref()
    }

    pub const fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }

    pub const fn updated_at(&self) -> &DateTime<Utc> {
        &self.updated_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantCreation {
    id: SubscriptionGrantId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    kind: SubscriptionGrantKind,
    reason: SubscriptionGrantReason,
    ends_at: DateTime<Utc>,
    granted_by_actor_id: ActorId,
}

impl SubscriptionGrantCreation {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        id: SubscriptionGrantId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        kind: SubscriptionGrantKind,
        reason: SubscriptionGrantReason,
        ends_at: DateTime<Utc>,
        granted_by_actor_id: ActorId,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            kind,
            reason,
            ends_at,
            granted_by_actor_id,
        }
    }

    pub const fn id(&self) -> SubscriptionGrantId {
        self.id
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn kind(&self) -> SubscriptionGrantKind {
        self.kind
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }

    pub const fn ends_at(&self) -> &DateTime<Utc> {
        &self.ends_at
    }

    pub const fn granted_by_actor_id(&self) -> ActorId {
        self.granted_by_actor_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionGrantRevocation {
    id: SubscriptionGrantId,
    billing_scope_id: BillingScopeId,
    subscriber_id: SubscriberId,
    plan_key: PlanKey,
    revoked_by_actor_id: ActorId,
    reason: SubscriptionGrantReason,
}

impl SubscriptionGrantRevocation {
    pub const fn new(
        id: SubscriptionGrantId,
        billing_scope_id: BillingScopeId,
        subscriber_id: SubscriberId,
        plan_key: PlanKey,
        revoked_by_actor_id: ActorId,
        reason: SubscriptionGrantReason,
    ) -> Self {
        Self {
            id,
            billing_scope_id,
            subscriber_id,
            plan_key,
            revoked_by_actor_id,
            reason,
        }
    }

    pub const fn id(&self) -> SubscriptionGrantId {
        self.id
    }

    pub const fn billing_scope_id(&self) -> BillingScopeId {
        self.billing_scope_id
    }

    pub const fn subscriber_id(&self) -> SubscriberId {
        self.subscriber_id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn revoked_by_actor_id(&self) -> ActorId {
        self.revoked_by_actor_id
    }

    pub const fn reason(&self) -> &SubscriptionGrantReason {
        &self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionGrantCreationOutcome {
    Created(Box<SubscriptionGrantRecord>),
    EndsAtNotFuture,
    CurrentPaidSubscription,
    ActiveGrant,
    BlockingInitialAttempt,
    PendingApprovedProcessorEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionGrantRevocationOutcome {
    Revoked(Box<SubscriptionGrantRecord>),
    AlreadyRevoked(Box<SubscriptionGrantRecord>),
    NotFound,
    Expired,
}

impl SubscriptionGrant {
    pub fn new(
        id: SubscriptionGrantId,
        plan_key: PlanKey,
        kind: SubscriptionGrantKind,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
        granted_by_actor_id: ActorId,
    ) -> Result<Self, SubscriptionGrantError> {
        if ends_at <= starts_at {
            return Err(SubscriptionGrantError::InvalidPeriod);
        }
        Ok(Self {
            id,
            plan_key,
            kind,
            starts_at,
            ends_at,
            granted_by_actor_id,
        })
    }

    pub const fn id(&self) -> SubscriptionGrantId {
        self.id
    }

    pub const fn plan_key(&self) -> &PlanKey {
        &self.plan_key
    }

    pub const fn kind(&self) -> SubscriptionGrantKind {
        self.kind
    }

    pub const fn starts_at(&self) -> &DateTime<Utc> {
        &self.starts_at
    }

    pub const fn ends_at(&self) -> &DateTime<Utc> {
        &self.ends_at
    }

    pub const fn granted_by_actor_id(&self) -> ActorId {
        self.granted_by_actor_id
    }
}
