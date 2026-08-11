# Schema version 1

`install.sql` is the authoritative fresh-install DDL for the provider-neutral
billing contract. Hosts copy it byte-for-byte into one immutable migration,
then add host identity, actor, credential, and target bindings in a separate
host migration.

`schema_contract::V1_INSTALL_SQL` embeds this exact file. The shared
read-only conformance entrypoint checks the canonical catalog fingerprint while
allowing explicitly host-prefixed tables, indexes, triggers, and foreign keys.
The package's fresh-install fixtures separately prove canonical locking,
function, trigger, nullable-shape, and bounded-provider-text behavior. Do not
edit version 1 after a host materializes it; introduce `schema/v2/` and forward
host migrations instead.
