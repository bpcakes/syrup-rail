use std::{error::Error, io};

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    CatalogIndexShape, IndexContract, REINDEX_TRANSITION_DETAIL, RENEWAL_DISPATCH_INDEX_CONTRACT,
    SUBSCRIPTION_HISTORY_INDEX_CONTRACT, SchemaConformanceAttemptError, V1_INSTALL_SQL,
    V1_TO_V2_PREFLIGHT_SQL, V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL, V1_TO_V2_UPGRADE_SQL,
    V2_CATALOG_FINGERPRINT, V2_CURRENT_SUBSCRIPTION_COLUMNS, V2_INSTALL_SQL, V2_TO_V3_UPGRADE_SQL,
    V3_CATALOG_FINGERPRINT, V3_INSTALL_SQL, V3_TO_V4_INDEX_SQL, V3_TO_V4_PREPARE_SQL,
    V3_TO_V4_UPGRADE_SQL, V3_TO_V4_VALIDATE_SQL, V4_CATALOG_FINGERPRINT, V4_INSTALL_SQL,
    V4_TO_V5_INCOMPATIBLE_ATTESTATION_AUDIT_SQL, V4_TO_V5_PREFLIGHT_SQL, V4_TO_V5_UPGRADE_SQL,
    V5_CATALOG_FINGERPRINT, V5_INSTALL_SQL, active_reindex_shadows, assert_schema_conforms,
    assert_v1_conforms, assert_v2_conforms, assert_v3_conforms, assert_v4_conforms,
    assert_v5_conforms, canonical_catalog_fingerprint_for_pool as canonical_catalog_fingerprint,
    load_billing_index_catalog, require_index_contract, require_supported_postgres_version_num,
    require_unchanged_active_reindex_shadows, retry_reindex_transition_once,
    validate_index_contract,
};
use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

/// Encodes fixture identity without decimal runs that can accidentally satisfy
/// the raw-card detector used by provider-reference value types.
fn opaque_fixture_uuid(id: Uuid) -> String {
    const NIBBLES: &[u8; 16] = b"abcdefghijklmnop";

    let mut encoded = String::with_capacity(32);
    for byte in id.as_bytes() {
        encoded.push(NIBBLES[usize::from(byte >> 4)] as char);
        encoded.push(NIBBLES[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[test]
fn opaque_fixture_uuid_is_unique_and_cannot_resemble_card_data() {
    let first = opaque_fixture_uuid(Uuid::from_u128(0));
    let second = opaque_fixture_uuid(Uuid::from_u128(1));

    assert_eq!(first.len(), 32);
    assert!(first.bytes().all(|byte| byte.is_ascii_lowercase()));
    assert_ne!(first, second);
    assert!(!syrup_rail::string_contains_raw_card_data(&first));
    assert!(!syrup_rail::string_contains_raw_card_data(&second));
}

mod fixtures;
mod storage_fixtures;
mod upgrade;
mod v1;
mod v2;
mod v3;
mod v4;
mod v5;
