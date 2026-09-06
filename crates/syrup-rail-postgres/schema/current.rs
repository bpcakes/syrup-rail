// One current install selection for integration fixtures and the SQLx gate.
// Keep this module independent of the query-owning crate so SQLx can provision
// its database before query macros have metadata. Historical artifacts and
// version-specific runtime assertions remain explicit and immutable.
pub const INSTALL_SQL: &str = include_str!("v6/install.sql");
