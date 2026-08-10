use std::{error::Error, io};

use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    V1_INSTALL_SQL, V1_TO_V2_PREFLIGHT_SQL, V1_TO_V2_RETRY_RECLASSIFICATION_AUDIT_SQL,
    V1_TO_V2_UPGRADE_SQL, V2_INSTALL_SQL, assert_v1_conforms, assert_v2_conforms,
    canonical_catalog_fingerprint,
};
use crate::test_support::{GatewayAccountFixture, TestDatabase, create_gateway_account};

mod fixtures;
mod storage_fixtures;
mod upgrade;
mod v1;
mod v2;
