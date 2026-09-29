# Billing addresses and exact payment diagnostics

## Outcome

Enable IdentityPro to send a customer-confirmed billing address on enrollment,
replacement, recovery, and subsequent renewals, and to inspect the processor's
actual payment response without changing financial state. Addressless historical
subscriptions remain usable. This work does not promise to cure processor fraud
declines.

This is the canonical producer plan for engineers implementing this feature on the
0.5.x line, starting from IdentityPro's production pin
`16801e5dc5fd2954eb1f5f696f91f53e8a55f26f`. Its consumer is the
[IdentityPro adoption plan](../../../identitypro/.agent/plans/20260928-billing-address-and-diagnostics.md).
The observed defects are missing address transport/durable snapshots and hidden
processor diagnostics. Archive the plan after delivery and handoff are verified;
do not maintain a separate planning dashboard or evidence ledger.

Completion signals: an exact 0.5.4 revision that descends from `16801e5` carries
addresses on every contact-bearing wire path, persists them in schema v5, scrubs
them with approval-exclusive locking, exposes a read-only diagnostic query, and
passes the verification commands below.

## Scope

In scope: address value and transport, durable method and attempt address
snapshots, schema v5, scrub/approval lock coordination for the new PII, and a
separate bounded diagnostic query API with a DB-only eligibility read. Host charges
share the snapshot type and are covered. The diagnostic API is for explicit operator
inspection, not automatic capture of every response. IdentityPro owns its
observation cache, authorization, rate budget and UI.

Non-goals: 3DS, network tokens, provider migration, gateway-rule changes, MCC
changes, new dunning or retry policy, charging the incident customer, address
verification services, NMI Customer Vault update/delete operations, crates.io
publication, and a new customer card-replacement feature in IdentityPro. Never
store PAN/CVV or issue a provider mutation to obtain diagnostic information.

## Progress

- [x] Inspect producer/consumer code and production investigation evidence.
- [x] Define ownership, compatibility, tasks and verification.
- [x] Independent audit of the first draft against code (2026-09-28); findings integrated below.
- [x] Address the revised-plan audit's NMI limit/source correction (2026-09-28).
- [ ] Implement and verify T-01 through T-03; planning is not implementation.

Restart checkpoint (2026-09-28): the main checkout
`/home/aa/Documents/syrup-rail` is on `feature/billing-address-diagnostics-0.5.4`
at `16801e5`, with this untracked plan and the modified Beads export. The work
branch is already created; first confirm pin ancestry, then start T-01. No production action,
commit, publication or deployment is authorized by this plan. Tracker IDs are
recorded below; the first safe task is `syrup-rail-e7h.1`.

## Surprises & Discoveries

- The first draft targeted `integration/0.5.3` (base `1c97ab6`, plan commit
  `304605b`). That branch lacks the production pin, the `v0.5.3` tag and seven
  commits. On that base, merchant
  renewals still went to v5 JSON, `Client::with_customer_receipts_disabled`
  did not exist, and IdentityPro would not compile. `release/0.5.3` (`c3f76de`)
  also lacks `16801e5`. The plan now starts from the pin.
- `master` and `integration/0.6.0` already declare 0.6.0, and each has its own
  unreleased `schema/v5` (attestation-constraint tightening); `integration/0.6.0`
  adds v6. See D-03 for the renumbering decision.
- Scrub and approval use different advisory keys on this line
  (`deletion.rs::lock_payment_method_scrub_domain` vs
  `enrollment_application.rs::lock_payment_method_domain`), so an approval can
  restore contact data during a scrub. `master` fixed this with
  `advisory_locks::lock_payment_method_domains`; git ancestry already includes the
  introducing commit `47786ea`, but the pin's scrub does not use it, so it must be
  ported by hand.
- A rate-limited Query API call writes the shared provider cooldown; a renewal that
  meets that cooldown is recorded `Failed` with
  `GatewayProviderRateLimitedBeforeSubmission`. Diagnostics are therefore not
  financially inert under load; see D-04.
- Renewals currently discard contacts; schema v4's exact assertion makes even
  nullable additions a stopped-writer cutover; late diagnostics cannot be inserted
  into immutable financial equality.
- The earlier 255-byte NMI address-line/city limits were attributed to the wrong
  schema. The v5 sale's `BillingAddressRest` documents 100-character address lines
  and a 50-character city; Classic sale/validate publish no maximum lengths for
  these fields. D-01 now distinguishes documented limits from local byte caps.

## Decision Log

- 2026-09-28: Proposed D-01 through D-04 from repository/incident evidence. Selected
  explicit diagnostic reads plus a host cache instead of capturing all responses or
  introducing a polling queue.
- 2026-09-28 (audit revision, supersedes the `integration/0.5.3` base and the
  provisional 0.6.0 version): operator chose the 0.5.x line. Base is the production
  pin `16801e5`; release version 0.5.4; this line owns schema v5 and mainline
  schemas are renumbered (D-03). Added scrub/approval coordination, NMI-format
  address bounds, address-only contacts, a typed diagnostic eligibility read,
  broadened diagnostic eligibility, and the diagnostic cooldown side effect.
- 2026-09-28 (independent plan review): added renewal submission and core renewal
  builder changes, v4/v5 test-harness split, explicit public re-exports and v4
  documentation updates, a diagnostic request carrying the expected operation,
  subscription-kind-only diagnostic targets, code bounds and stable outcome strings,
  missing-method reservation behavior, and lock-order rustdoc duties.
- 2026-09-28 (operator confirmation): implement on a work branch based on the
  exact IdentityPro pin `16801e5dc5fd2954eb1f5f696f91f53e8a55f26f`.
  `integration/0.5.3` and its committed first-draft plan are historical inputs,
  not the implementation base or producer handoff.
- 2026-09-28 (revised-plan audit correction): trace v5 sale through
  `PaymentSaleRequest` → `PaymentSaleBase.billing_address` → `BillingAddressRest`.
  Use matching conservative NMI address byte caps across wire routes and in the
  consumer, retain the provider-neutral core/storage cap, and remove the claim
  that local validation guarantees processor acceptance. Add per-route boundary
  tests, including multibyte input.

## Outcomes & Retrospective

Planning only. No code, schema, gateway settings or production records changed.
Implementation, migration rehearsal, sandbox behavior and release acceptance
remain unverified. Plan review is not independent proof of the future
implementation, and no delivery task is closed by it.

Revised-plan checks (2026-09-28): the critical-profile plan validator passed with
one isolated-task warning for the intentionally independent T-03; local links and
whitespace checks passed. Public NMI request schemas were read without calling
payment endpoints. No application tests or provider acceptance checks ran.

## Current-state evidence

All paths are at `16801e5` unless another revision is named.

- **Fact:** core `crates/syrup-rail/src/gateway_value.rs::BillingContact` (names and
  email, 255-byte `MAX_BILLING_CONTACT_FIELD_BYTES`) and
  `attempt/snapshots.rs::BillingContactSnapshot` carry no address.
  `BillingContact::new` returns `BillingContactError::Empty` when all three fields
  are absent. `BillingContact::into_parts` returns a 3-tuple; its callers are
  `crates/syrup-rail-nmi/src/adapter.rs` and IdentityPro
  `identitypro-billing/src/gateway/sandbox.rs::without_email`.
- **Fact:** `attempt/fingerprint.rs` excludes contacts. Exact replay compares the
  derived structural contact snapshot in `attempts/initial/support.rs`,
  `attempts/recovery.rs`, `attempts/payment_method_replacement.rs` and
  `host_charges.rs`; renewal admission compares the request at
  `attempts/renewal.rs` admission.
- **Fact:** there are five attempt kinds (`schema/v4/install.sql` `attempt_kind`):
  initial, recovery, payment-method replacement, renewal and host charge.
  `ChargeHostTarget` carries an optional `BillingContact`.
- **Fact:** the public core renewal builders
  (`crates/syrup-rail/src/renewal.rs`: `SubscriptionRenewalLockedTerms`,
  `from_locked_subscription`, `from_locked_subscription_terms`) build
  `BillingContactSnapshot::new(None, None)`, and
  `enrollment_application/renewal.rs::submit_admitted_subscription_renewal` builds
  its `GatewaySaleRequest` with `None` as the contact. Adding checkout fields alone
  will not populate renewals. Renewals never replay (reservation inserts with
  `ON CONFLICT DO NOTHING`).
- **Fact (wire routing):** `syrup-rail-nmi-client/src/client.rs::sale_wire` sends
  vault-creating sales (`customer_vault=add_customer`: initial and recovery) and
  merchant-initiated renewals (`StoredCredential::RecurringMerchant`) through the
  Classic form (`classic_sale`); only other sales (host-charge `OneTime`) use
  `POST /api/v5/payments/sale`. Replacement uses Classic `type=validate` with
  `add_customer` (`client/form.rs::classic_store_payment_method_params`); there is
  no v5 validate. Classic contact fields are `first_name`, `last_name`, `email`
  (`form.rs::classic_sale_params`); v5 sends `billing_address.{first_name,
  last_name,email}` (`client/v5.rs::billing_address_json`). Customer-receipt
  suppression applies to Classic and v5 (`client.rs` `customer_receipts_disabled`).
  CIT/MIT fields are set in `classic_sale_params` (`stored_credential_indicator`,
  `initiated_by`, `initial_transaction_id`).
- **Fact:** raw `syrup-rail-nmi-client/src/requests.rs::BillingContact` has public
  fields and is embedded in public `SaleRequest`/`StorePaymentMethodRequest`.
  Adding a field breaks struct literals; all literals are inside this repository
  (`nmi/src/adapter.rs`, `nmi-client/src/public_api_tests.rs`, and
  `nmi-client/src/client/tests/{request_bounds,form_requests,customer_receipts}.rs`).
  IdentityPro builds no raw-client literals. The raw
  client bounds names at 256 and email at 320 bytes and the encoded request at
  16 KiB (`client.rs`, `client/request_budget.rs`);
  `client/validation.rs::validate_sale_request_size` measures merchant renewals as
  JSON although `sale_wire` sends them as a Classic form (the wire-time
  `ensure_form_request_is_bounded` still catches overflow). The form `Debug` allowlist
  (`form.rs::nmi_form_param_is_debug_safe`) redacts unknown keys.
- **Fact:** enrollment, recovery and replacement create NMI Customer Vault records
  (`add_customer`); renewals charge by `customer_vault_id`. The library has no vault
  update/delete operation.
- **Fact:** apart from the scrub, `enrollment_application.rs::upsert_payment_method`
  is the only writer of method contact columns and overwrites `billing_name`/`billing_email` on conflict,
  NULL included. Its callers are initial, recovery and replacement approval.
- **Fact (locks):** approval writers take `lock_payment_method_domain`
  (account:subscriber advisory key) then `lock_subscription_aggregate`. Renewal
  reservation (`attempts/renewal.rs::reserve_subscription_renewal_in_transaction`)
  reads an unlocked locator (`subscriber_id`, `plan_key`), takes only the
  aggregate lock, then rereads the subscription `FOR SHARE`; it reads no method
  row. Renewal admission holds the aggregate and reads the method row `FOR SHARE`
  requiring `status = 'active'` (`enrollment_application/renewal.rs`). The scrub
  (`deletion.rs::scrub_subscriber_billing_data`) takes only
  `lock_payment_method_scrub_domain` (a prefixed key) per affected account.
  `payment_method_metadata/storage.rs::lock_candidate` is the only path taking
  scrub domain, approval domain, then aggregate. On `master`,
  `advisory_locks::lock_payment_method_domains` makes the scrub take the approval
  key so approvals cannot restore data mid-scrub; the pin lacks that coordination.
  `billing_subscriptions.gateway_account_id` exists. No path takes the approval key
  after the aggregate or row locks. The host coordinator contract is in
  `src/transactions.rs`.
- **Fact:** schema v4 is shipped and materialized by IdentityPro. Its runtime
  assertion fingerprints every column including `ordinal_position`
  (`src/schema_contract.rs`, hand-computed `V4_CATALOG_FINGERPRINT`), ignores
  constraints outside the `billing_` name prefix and rejects `NOT VALID` among
  `billing_%` constraints; adding columns invalidates old startup assertions. v2→v3 was one stopped-writer transaction; v3→v4 was staged
  (`schema/v4/README.md`). `tools/sqlx-gate/src/main.rs` and
  `src/test_support.rs::TestDatabase::start` install v4; v4 conformance
  (`schema_contract/tests/v4.rs`) and chain tests
  (`paid_trial_dunning_tests/migration.rs`, `schema_contract/tests/upgrade.rs`) use
  them. The v4 assertion is an explicit re-export in `src/lib.rs`, and
  `scripts/check-public-api.sh` forbids wildcard re-exports. v4 is named as current
  in `examples/host_integration.rs`, `docs/public-api.md`, both `AGENTS.md` files and
  the READMEs. No v4 trigger guards contact columns.
- **Fact:** the 2026-09-28 production investigation found correct merchant/used
  indicators and original-transaction linkage, absent billing address, AVS `0`,
  initial CVV `M`, and TSYS `59` beneath NMI `253` on two renewals. The operator
  reports NMI AVS/CVV rejection rules and Fraud Prevention disabled. Do not copy
  customer identifiers or card data into tests or this plan.
- **Inference:** missing address is an integration gap, not a proven explanation
  of those declines; other addressless renewals succeeded.
- **Fact (diagnostics):** `processor_charges/storage.rs` compares financial
  observations immutably, backed by the `billing_processor_charge_evidence_immutable`
  trigger (function `billing_guard_processor_charge_evidence_update`). `payment_method_metadata.rs` separates display enrichment from financial
  evidence, uses short timed read transactions, loads shared cooldowns before I/O,
  bounds the query at 10 s, revalidates configuration after I/O, and on
  `GatewayError::RateLimited` persists a 60 s provider cooldown
  (`enrollment_application/outcome_support.rs::persist_bound_provider_rate_limit_cooldown`,
  which takes the account row `FOR UPDATE`). A renewal that finds an active cooldown
  is resolved `Failed` with `GatewayProviderRateLimitedBeforeSubmission`
  (`subscription_billing_service/renewal.rs::resolve_renewal_cooldown`); it is not
  marked past due, but it advances the rate-limit retry ladder
  (`syrup-rail/src/renewal.rs::rate_limit_retry_after_seconds`: 24 h after 5).
- **Fact:** declined attempts keep the NMI transaction ID (Classic and v5 response
  parsing; `attempts/transitions.rs` persists `gateway_transaction_id`); errors and
  timeouts carry none. Statuses: pending, approved, declined, unknown,
  review_required, failed. Late-approval review can move declined/failed to
  `review_required`.
- **Fact:** the existing exact query parser (`client/response/xml.rs`) reads only
  transaction-level fields and ignores action elements; lifecycle code selects actions by
  recency. No query path parses `processor_response_*`, `avs_response` or
  `csc_response` (the Classic payment-response parser reads `avsresponse` and
  `cvvresponse`). `GatewayQueryRequest` carries only a transaction and/or order ID.
  Host-charge attempts have `plan_key IS NULL` and are excluded from payment history.
  The cooldown loader (`gateway_accounts.rs::load_gateway_cooldown`) returns booleans.
- **Fact:** `billing_portal.rs::subscription_payment_history_page` is provider-free
  by design: no transaction IDs or response data, and its cursor holds only
  `(created_at, attempt_id)`.
- **Fact:** `deletion.rs` documents that the caller owns deletion admission and
  stabilizes billing-row creation; it says nothing about diagnostic reads. Scrub
  nulls `gateway_response_text` but keeps `gateway_transaction_id`.
- **Fact:** the gateway port's defaulted `query_payment_method_metadata`
  (`syrup-rail/src/gateway/port.rs`) documents that decorators must forward it.

Official sources checked 2026-09-28:
[Classic payment fields](https://docs.nmi.com/reference/transactions-processing)
define `address1`, `address2`, `city`, `state` (Format: CC), `zip`, `country`
(ISO 3166-1 alpha-2). Its [downloadable OpenAPI](https://docs.nmi.com/reference/transactions-processing.md)
has both `CreditCardSaleRequest` and `CreditCardValidateRequest` include
`BaseTransaction`; these address fields have no declared maximum lengths.
The [v5 sale OpenAPI](https://docs.nmi.com/reference/create-sale-v5.md) resolves
`PaymentSaleRequest` through `PaymentSaleBase.billing_address` to
`BillingAddressRest`: `address1`/`address2` maxLength 100, `city` 50, `state` 50,
`zip` 20 and `country` 2. These are character limits, not UTF-8 byte limits.
The [vault billing-address endpoint](https://docs.nmi.com/reference/add-billing-address-v5)
also lists 100/100/50/50/20/2, but is not the operation used by this integration.
The local caps in D-01 are a conservative contract, not undocumented Classic
provider limits or proof of processor acceptance. The
[Query API](https://docs.nmi.com/reference/query) returns transaction-level
`avs_response`, `csc_response`, `currency` and action-level `action_type`,
`amount`, `success`, `response_code`, `response_text`, `processor_response_code`,
`processor_response_text`; `transaction_id` may also match a Subscription ID.
[Rate limiting](https://docs.nmi.com/reference/rate-limiting) is shared by Payment
and Query APIs (HTTP 429). Unknown: whether a `customer_vault_id` sale applies the
vault's stored address; the design sends the address explicitly either way.

## Decisions and design

### D-01 — Address is part of the durable request (accepted, 2026-09-28)

**Core value.** Add `BillingAddress` to `gateway_value.rs` with `line1` (required),
optional `line2`, `city`, `region`, `postal_code`, and required `country`. Trim all
fields; empty optional fields become absent; reject control characters and any
field over `MAX_BILLING_CONTACT_FIELD_BYTES` (255). `country` must be two ASCII
letters and is stored uppercase. Postal codes stay text (leading zeros preserved).
`Debug`/`Display` expose only `has_*` flags. The core is country-neutral;
IdentityPro requires a complete U.S. address.

**Contact API.** Keep `BillingContact::new(first, last, email)` semantics. Add
`BillingContact::with_address(self, BillingAddress)`,
`BillingContact::from_address(BillingAddress)` (an address-only contact is
non-empty), and `address()`. Replace the 3-tuple `into_parts` with
`into_parts(self) -> BillingContactParts` (first, last, email, address) so every
caller must handle the address at compile time; this intentionally breaks
IdentityPro's `without_email`, which T-01 there updates.
`BillingContactSnapshot` gains the address; `from_billing_contact` copies it;
`is_empty` is true only when names, email and address are all absent; equality
stays derived (structural); add a public `BillingContactSnapshot::address()` so
hosts can rebuild continuation contacts infallibly. Fingerprints are unchanged.

**NMI boundary.** Raw `requests.rs::BillingContact` gains
a public optional `address` field (raw fields `address1`, `address2`, `city`,
`state`, `zip`, `country`). `client/validation.rs` rejects before network I/O:
blank or >100-byte `address1`, >100-byte `address2`, >50-byte `city`, a present
`state` not exactly two ASCII alphanumerics, a present `zip` over 20 bytes or
outside ASCII alphanumerics/space/hyphen, or `country` not two uppercase ASCII
letters. Apply the same address rules to Classic sale (vault-creating and merchant
renewal), Classic validate and v5 sale. These UTF-8 byte caps are deliberately
conservative relative to the v5 character limits; the two-character state format
and ZIP character set are local restrictions. Classic's undocumented maxima stay
unknown. Keep the core/schema's independent 255-byte cap unchanged.
`request_budget.rs` counts the new fields, and `validate_sale_request_size`
measures merchant renewals as the Classic form they are sent as.

This checks the library's accepted format and request budget; NMI or TSYS may
still reject a syntactically valid address or payment. An address the core accepts
but the local NMI validator rejects becomes a terminal before-submission failure
after reservation, and a corrected retry needs a new key. Rustdoc must tell hosts
to pre-validate to these rules; IdentityPro uses 100-byte lines, a 50-byte city,
the U.S. region set and normalized ZIP/ZIP+4 before reserving. Provider acceptance
must not be inferred from synthetic boundary tests.

**Wire mapping.** Classic sale (initial, recovery, merchant renewal) and Classic
validate (replacement) add the six Classic fields when present. v5 sale (host
charges) adds `billing_address.{address1,address2,city,state,zip,country}`;
the sale request schema above confirms these names (the separate webhook address
schema uses different names and is not the request contract). Do not add address
keys to the form `Debug` allowlist. CIT/MIT
fields, vault action and `customer_receipt` suppression are unchanged. Renewals
send an address-only contact from the reserved snapshot: in
`submit_admitted_subscription_renewal`, pass `BillingContact::from_address` when the
snapshot has an address and `None` otherwise, so addressless renewal wire bytes are
unchanged (names/email are not sent on renewals, as today). The explicit per-attempt address is authoritative;
whether NMI would also apply a vault-stored address is not relied on, so wire tests
(not sandbox inspection) prove renewal transport.

### D-02 — Durable snapshots, locking and scrub (accepted, 2026-09-28)

**Columns.** Append six nullable `text` columns to both `billing_payment_methods` and
`billing_payment_attempts`: `billing_address_line1`, `billing_address_line2`,
`billing_address_city`, `billing_address_region`, `billing_address_postal_code`,
`billing_address_country`. Add `billing_payment_methods_billing_address_valid` and
`billing_payment_attempts_billing_address_valid`: all six NULL, or `line1` and
`country` non-NULL, `country ~ '^[A-Z]{2}$'`, every non-NULL field nonblank after
`btrim` and at most 255 octets. No inferred backfill; historical rows stay NULL.

**Method writes.** `upsert_payment_method` writes the address as a whole value: a
command carrying an address sets all six columns; a same-reference conflict whose
command carries no address keeps all six existing columns (six `CASE` expressions
sharing the predicate `EXCLUDED.billing_address_line1 IS NULL`, never per-column
`COALESCE`, which could mix two addresses and still satisfy the CHECK). A new
reference inserts only what its command carries, so a new card never inherits an
old address. Names and email keep today's overwrite-on-conflict behavior, so a
same-reference addressless re-approval can pair new names with the retained
address; this is accepted because NMI issues a new vault ID per `add_customer`, so
same-reference upserts are rare, and the address belongs to the card.

**Attempt writes and replay.** All five attempt inserts write the snapshot address
and `attempts/persistence.rs` (`PAYMENT_ATTEMPT_SELECT`, `payment_attempt_from_row`)
reads it. Exact replay compares the full snapshot: a same-key command whose
address differs from the stored one, including a pre-v5 attempt with NULL address
replayed with an address, is an idempotency conflict before provider I/O. An
addressless replay of an addressless attempt still matches.

**Renewal reservation.** In `reserve_subscription_renewal_in_transaction`, extend
the unlocked locator to also read the subscription's `gateway_account_id`; take
`lock_payment_method_domain(subscriber_id, gateway_account_id)` before
`lock_subscription_aggregate`; keep the existing `FOR SHARE` subscription reread and
gateway-identity rejection, and reject (existing mismatch path) if the account
changed between locator and lock. Then read the selected method's address with
admission's filter (`id = subscription.payment_method_id`, same scope, subscriber
and account, `status = 'active'`, `FOR SHARE`) and pass it to the core builder: add
an optional billing address to `SubscriptionRenewalLockedTerms` (builder method,
listed as an API change) that `from_locked_subscription_terms` uses instead of
`BillingContactSnapshot::new(None, None)`. If no active method row matches, reserve
with an absent address and let admission's existing active-method check fail it as
today; add no new rejection. Never
add the address read to renewal admission (which already holds the aggregate) or
to submission; submission uses only the attempt snapshot. Resulting global order:
scrub domain, then approval domain, then subscription aggregate, then rows; no path
takes an advisory method lock after the aggregate. Rustdoc on the public
`reserve_subscription_renewal_in_transaction` must state that callers hold no
aggregate or billing row locks; rustdoc on the scrub must state that the host takes
its subject lock before calling (the `src/transactions.rs` coordinator contract).

**Scrub.** `scrub_subscriber_billing_data` keeps its scrub-domain locks, then takes
`lock_payment_method_domain` for every affected account (ascending, deduplicated)
before any row update, porting `master`'s `advisory_locks::lock_payment_method_domains`
by hand (ancestry tools report its introducing commit as present). It clears all six address columns on methods and
attempts. Replace the denylist projections in the scrub tests with assertions that
enumerate every retained column, so an omitted PII column fails.

### D-03 — Schema v5 and the 0.5.4 release (accepted, 2026-09-28)

Add `schema/v5/install.sql`, `schema/v5/upgrade_from_v4.sql` and
`schema/v5/README.md` (cutover instructions following the v3/v4 READMEs), plus
`assert_runtime_schema_v5_compatible` and its fingerprint. Keep
`assert_runtime_schema_v4_compatible` exported unchanged so hosts can check the
pre-cutover state; keep v1–v4 immutable. `install.sql` appends the new columns so
fresh-install and upgrade catalogs (including `ordinal_position`) match. The
upgrade is one transaction: short `lock_timeout`, `ADD COLUMN` (metadata-only) and
validated `ADD CONSTRAINT ... CHECK` (a scan of each table under `ACCESS EXCLUSIVE`;
the assertion rejects `NOT VALID`). IdentityPro measures the scan on a restored
production-sized copy; if it exceeds the maintenance window, stop and replan a
staged cutover. Switch `tools/sqlx-gate` and `TestDatabase::start` to v5; add
`TestDatabase::start_v4` and a v4-to-v5 upgrade helper; point the v4 conformance
and chain tests at `start_v4` and extend the chains to v5; add
`schema_contract/tests/v5.rs` (fresh equals upgraded equals a hand-computed
`V5_CATALOG_FINGERPRINT`) registered in `schema_contract/tests.rs`. Add explicit
re-exports in `src/lib.rs` for the v5 assertion and every new public item
(`BillingAddress`, `BillingContactParts`, the raw address type, and the diagnostic
types and functions). Update the v4-as-current wording in
`examples/host_integration.rs`, `docs/public-api.md` (also its 0.5.2 title), the
root and `crates/syrup-rail-postgres` `AGENTS.md` invariants and the READMEs.

Version: 0.5.4 for all four crates, internal requirements, `Cargo.lock` and
`CHANGELOG.md`; fold the pin's existing `[Unreleased]` entries into 0.5.4. No
`v0.5.4` tag exists; the `release/0.5.4` branch (`8c3eb6d`) is an ancestor of the
pin and can be fast-forwarded to the tested revision. `release/0.5.3` (`c3f76de`) carries
0.5.3 release records but lacks `16801e5`; the release manager merges those
records into the work branch before release. This is a deliberate, operator-authorized
exception to `docs/releasing.md` ("do not merge unreleased features into a patch
release"): 0.5.4 needs a stopped-writer schema cutover and breaks raw
`BillingContact` literals, so it is not a Cargo-semver-compatible patch. Delivery to
IdentityPro is by exact git revision (it pins `=` version and `rev`). crates.io
publication is out of scope; publishing 0.5.4 would need a separate release
decision because `^0.5` dependents would receive the cutover (0.5.3 is on
crates.io). The CHANGELOG labels every break: the host schema cutover, raw
`BillingContact` literals, the `into_parts` return type, and the renewal builder
change if it alters a public signature.

Schema numbering: this line owns schema v5 (v4 plus addresses). Operator decision
2026-09-28: mainline schemas move up (the unreleased `master`/`integration/0.6.0`
contracts renumber, v6 becoming v7), and mainline must upgrade from this v5 before
IdentityPro adopts it. That renumbering is mainline work outside this plan.

### D-04 — Diagnostic queries are observational (accepted, 2026-09-28)

**Interface.** Add `GatewayTransactionDiagnosticsRequest` (required transaction ID,
expected operation `Sale` or `Validate`, amount, currency, optional expected order
ID) and a defaulted port method
`PaymentGateway::query_transaction_diagnostics(GatewayTransactionDiagnosticsRequest)`
returning a typed `GatewayTransactionDiagnostics` whose default is `Unsupported`
without I/O. The adapter performs action selection and the order-ID comparison,
because `GatewayQueryRequest` cannot carry the expected operation.
Document that decorators must forward it (as for `query_payment_method_metadata`)
and add a `ResolvedGateway` passthrough. Add separate raw query/parser types in the
NMI client and an NMI adapter implementation. In `syrup-rail-postgres`, add:

- `payment_attempt_diagnostic_eligibility(pool, scope, subscriber, plan, attempt_ids)`:
  DB-only, at most the payment-history page size, returns `Eligible` or
  `Ineligible(UnsupportedKind | NotSubmitted | Pending | NoTransactionId)` per owned
  attempt, first matching reason in that order, and omits unowned IDs. It never
  returns transaction IDs.
- `query_payment_attempt_diagnostics(pool, resolver, target, deadline)` where the
  target is scope, subscriber, plan and attempt ID. Outcomes: `Observed(PaymentAttemptDiagnostics)`,
  `Ineligible(reason)`, `NotFound` (no owned attempt), `ProviderTransactionNotFound`,
  `Unsupported`, `CooldownActive`, `RateLimited`, `TimedOut`, `ConfigurationChanged`,
  `TargetChanged`, `Unavailable(reason)`. Every outcome and reason has a stable
  `as_str()` value that hosts may persist. Storage failures and a rate-limit whose
  cooldown write fails are `Err` values, as in the metadata API
  (`RateLimitCooldownPersistenceFailed`).

**Eligibility.** Initial, recovery, replacement and renewal attempts with
`submitted_at` and `gateway_transaction_id` present and status other than `pending`
(approved, declined, failed, unknown, review_required), including unpaid
subscriptions. Host-charge attempts have no plan and are `UnsupportedKind`;
IdentityPro does not need them. No payment-method row is required.

**Flow.** Read the target in a short timed transaction; resolve its account and
provider through the current canonical `billing_gateway_accounts.gateway_configuration_id`
(never the attempt's stale configuration and never a default-account fallback);
load cooldowns and return `CooldownActive` without I/O when one applies; make one
exact query by the stored transaction ID with the existing 10 s bound, outside any
database transaction; revalidate target ownership, account, provider and
configuration after I/O (`ConfigurationChanged` on rotation), and confirm the
attempt's transaction ID and status are unchanged (`TargetChanged` otherwise, since
late-approval review can rewrite them). The explicit deadline bounds the whole call:
the query runs for the earlier of 10 s and the deadline minus a reserve for post-I/O
revalidation and cooldown bookkeeping, and the function returns `TimedOut` rather
than starting I/O it cannot finish, so a caller's deadline never cancels between
provider I/O and the cooldown write. The consumer passes 15 s. On
`RateLimited`, reuse the existing provider-cooldown bookkeeping (the only permitted
library write); generalize `payment_method_metadata/storage.rs::record_provider_cooldown`
so it does not require a metadata command. Never query by order ID; the adapter
compares a returned `order_id` with the expected order ID when both exist and
treats a mismatch as `Unavailable`.

**Parsing.** Require exactly one transaction element whose ID equals the requested ID.
Select the original action by kind: `sale` for initial, recovery, renewal and host
charge; `validate` for replacement. The action amount must equal the attempt amount
(0.00 for validate) and the transaction currency, when present, the attempt
currency. Exactly one match is selected; zero or several matching actions, a wrong
ID, or settlement/refund/void-only data yield `Unavailable`; never pick by recency.
Missing optional fields yield a partial observation, never invented values. Code
fields (gateway and processor codes, AVS, CSC, action type) are at most 64 bytes;
an oversized code is omitted and the observation marked partial. Text fields pass
through `GatewayDiagnostic`, which sanitizes and truncates to 512 bytes. Reuse
the envelope and selector primitives in `client/response/xml.rs`; keep existing
financial parsing byte-stable. Never extract name, address, email, card or
signature fields from the response.

**Result.** `PaymentAttemptDiagnostics` carries the target descriptor (scope,
subscriber, plan, attempt ID, attempt kind, gateway account ID, the
`gateway_configuration_id` actually queried, provider key), the selected action
type, optional `gateway_response_code`/`gateway_response_text`,
`processor_response_code`/`processor_response_text`, `avs_response`,
`csc_response`, source `nmi_query_api`, completeness, and `observed_at` from the
database clock after I/O. Values use redacted formatting with an explicit protected
accessor. Missing never means
match, mismatch, disabled or a financial decision.

**Side effects and host obligations (rustdoc on both functions).** A diagnostic call
can extend the shared 60 s provider cooldown when NMI rate-limits it; during that
cooldown, renewals are recorded `Failed`/`GatewayProviderRateLimitedBeforeSubmission`
(not past due) and advance the rate-limit retry ladder, and enrollments are
refused. The host must bound total diagnostic volume. The functions do not consult
scrub state: scrubbed attempts keep their transaction ID, so the host must not call
them for a subscriber that is scrubbed or being scrubbed, and must purge any cached
observation in the same transaction as its own scrub. Do not add these fields to
`ProcessorEvidence`, write attempts, charges or subscriptions, change
approval/entitlement decisions, or add a background queue.

### Feature-specific privacy assessment

Protected data: confirmed billing address and provider diagnostic text. Public
inputs and provider responses are untrusted; boundaries are host-to-command,
provider-to-parser and database-to-authorized-operator. Existing bounded values,
redacted formatting, host authorization and deletion scrubbing are sufficient; no
new encryption key, HMAC or security subsystem is proposed. Store address only
where immutable request reconstruction and current-method use need it, and clear it
with the approval-exclusive scrub (D-02). Provider-side copies are outside local
deletion: `add_customer` stores the address in the NMI Customer Vault with names
and email, and NMI transaction history retains it; this library offers no vault
deletion, so provider-side deletion stays a host/operator responsibility that the
host's privacy notice must describe. This does not protect against root/database
administrators or guarantee provider truth. Follow `docs/security/threat-model.md`.

## Execution graph

Task IDs are local to this producer plan. T-02 depends on T-01. T-03 needs no schema
change and can be implemented independently in principle, but it shares
`gateway/port.rs`, the NMI adapter and client modules with T-01, so use sequential
ownership (T-01, then T-03, or one owner). IdentityPro adoption requires all three
at one tested 0.5.4 revision. Critical path: verify the pin-based branch, address transport,
durable v5 delivery, then host adoption.

### T-01 — Commands and NMI requests carry a confirmed billing address
- Outcome: Address-bearing and legacy addressless commands map correctly through all five attempt kinds on the routes production uses.
- Context: D-01. Production renewals use Classic, not v5. The work branch already exists; confirm `git merge-base --is-ancestor 16801e5dc5fd2954eb1f5f696f91f53e8a55f26f HEAD` succeeds before implementation.
- Changes: Core `gateway_value.rs` (`BillingAddress`, builder/accessor, `from_address`, `BillingContactParts`), `attempt/snapshots.rs` (address field and `address()`), snapshot builders in `enrollment.rs`, `recovery.rs`, `payment_method_update.rs`, `host_charge.rs`; raw `requests.rs`, `client/form.rs` (`classic_sale_params`, `classic_store_payment_method_params`), `client/v5.rs` (`billing_address_json`), `client/validation.rs` (address rules and `validate_sale_request_size` encoding), `client/request_budget.rs`; NMI `adapter.rs` contact mapping; raw-literal updates in `nmi-client/src/public_api_tests.rs` and `nmi-client/src/client/tests/{request_bounds,form_requests,customer_receipts}.rs`; explicit re-exports for new public items; READMEs and public API docs.
- Depends on: none
- Verify: Core normalization, bounds, redaction, address-only contacts and structural snapshot equality; historical fingerprint bytes unchanged. Classic sale (initial, recovery, merchant renewal) and Classic validate (replacement) wire assertions for every address field; v5 host-charge sale mapping; each alongside unchanged `stored_credential_indicator`/`initiated_by`/`initial_transaction_id` and `customer_receipt` suppression (`client/tests/customer_receipts.rs`); old addressless wire bytes unchanged; address keys redacted in form `Debug`; merchant-renewal size measured as a Classic form. On each wire route, test address lines at 100/101 bytes, city at 50/51, ZIP at 20/21, state/country format and multibyte strings whose character count fits but byte count exceeds the cap; malformed fields and encoded-budget overflow fail before network I/O. The consumer's stricter U.S. profile must fit these same bounds.
- Recovery: Do not deploy this partial library change. Constructor-based callers keep compiling; `into_parts` and raw literal breaks are intentional and documented for coordinated adoption.
- Done when: Synthetic request tests show exact address transport for initial, recovery, replacement, merchant renewal and host-charge intents without PAN/CVV exposure or fingerprint changes, on a branch descending from `16801e5`.

### T-02 — Address survives reservation, renewal, reconciliation and deletion
- Outcome: A populated v4 database upgrades in place, v5 payments use the exact reserved address, and scrubs cannot be undone by concurrent approvals.
- Context: D-02 and D-03.
- Changes: `schema/v5/{install.sql,upgrade_from_v4.sql,README.md}`; `src/schema_contract.rs` (v5 fingerprint and `assert_runtime_schema_v5_compatible`) with explicit re-export; five attempt inserts and `attempts/persistence.rs`; `upsert_payment_method`; `attempts/renewal.rs` reservation locking, address read and rustdoc; core `renewal.rs` locked-terms address and snapshot; `enrollment_application/renewal.rs::submit_admitted_subscription_renewal` contact; `deletion.rs` scrub locking, columns and rustdoc; `tools/sqlx-gate`, `TestDatabase::start`/`start_v4`, v4/v5 conformance and chain tests, `.sqlx` metadata and immutability checks; `examples/host_integration.rs`, `docs/public-api.md`, both `AGENTS.md` files, CHANGELOG, READMEs and synchronized 0.5.4 versions.
- Depends on: T-01
- Verify: Real PostgreSQL populated v4-to-v5 upgrade equals fresh v5 (catalog and assertion); unchanged financial IDs, statuses, fingerprints and evidence; same-key changed address (including NULL-address pre-v5 attempt replayed with an address) conflicts without provider I/O; approved replacement updates the next renewal's snapshot and failed replacement preserves the old method/address; a same-reference upsert without address keeps the whole prior address; a reserved renewal cannot borrow a newer address; a scripted gateway shows the reserved address reaches the renewal `GatewaySaleRequest` and an addressless reservation sends `None`; a locator-vs-locked account mismatch is rejected; a reservation without an active method row stores no address and admission fails as today; deterministic concurrency tests show approval-vs-scrub and renewal-reservation-vs-scrub serialize with no restored PII and no lock inversion; scrub tests enumerate every retained column.
- Recovery: Transaction rollback before upgrade commit; after commit keep writers stopped and roll forward with a v5-aware build. Never drop columns, reset the database or rewrite historical migrations.
- Done when: All contact codecs and the five attempt kinds preserve optional address, absent old rows remain usable, the scrub is approval-exclusive and complete, and the v5 upgrade passes on a populated fixture.

### T-03 — Operators can query exact processor and verification results safely
- Outcome: Hosts can list which attempts are diagnosable and retrieve diagnostics for a historical payment without changing its financial state.
- Context: D-04.
- Changes: Raw query/parser types and tests in `syrup-rail-nmi-client`; `GatewayTransactionDiagnosticsRequest` and `query_transaction_diagnostics` on the core port with default and `ResolvedGateway` passthrough; NMI adapter implementation with action selection and order-ID comparison; PostgreSQL diagnostic module (`payment_attempt_diagnostic_eligibility`, `query_payment_attempt_diagnostics`) reusing metadata timeout/cooldown handling with a generalized `record_provider_cooldown`; explicit re-exports; public API docs including the side-effect, pre-validation and host-obligation rustdoc.
- Depends on: none
- Verify: Synthetic exact-query fixtures return separate gateway 253 / processor 59, AVS `0` and missing CSC; wrong or multiple transactions, conflicting or zero matching sale/validate actions, settlement/refund-only data and order-ID mismatch yield `Unavailable`; hostile or oversized text is sanitized and bounded, and oversized codes yield a partial observation; a sale followed by settlement/refund selects only the sale; response name/address/email fields never surface; stable `as_str()` values are covered. Real storage tests cover ownership (`NotFound`), eligibility for each status and kind with reason precedence (host charge `UnsupportedKind`), unpaid history, `TargetChanged` after a late-approval rewrite, `observed_at` from the database clock, a short deadline yielding `TimedOut` without I/O and never interrupting a cooldown write, active cooldown returning without I/O, a 429 extending the provider cooldown and a following renewal recorded as the documented readiness failure, configuration rotation before and during the query, concurrent scrub left intact, and zero financial/contact writes or provider mutations; existing approval-reconciliation tests unchanged.
- Recovery: Hosts can stop calling this optional observation API; provider/query failures return typed outcomes without financial writes; no data migration is required.
- Done when: IdentityPro can list eligibility and query an owned historical attempt at the same tested revision as T-02, receiving truthful bounded diagnostics including explicit unsupported/missing/ineligible results.

## Verification

Implementation commands from `/home/aa/Documents/syrup-rail`, with the repository
PostgreSQL 18 test configuration available: `cargo test -p syrup-rail`,
`cargo test -p syrup-rail-nmi-client`, `cargo test -p syrup-rail-nmi`, and
`cargo test -p syrup-rail-postgres`. Use focused existing test modules while
iterating, then `scripts/jig check test`, `scripts/jig check sqlx`,
`scripts/check-schema-immutability.sh` and `scripts/check-public-api.sh`. Note that
the immutability script only protects directories under ancestor tags, so review
v1–v4 diffs manually as well. Run the release preflight in `docs/releasing.md`
(`scripts/check-release.sh 0.5.4 --allow-dirty` and the listed checks) for the
later release action. Record exact revision and actual results; fixtures prove
behavior, not live NMI acceptance.

Plan-only validation: run
`python ~/.agents/skills/planning-workflow-v2/scripts/validate_plan.py .agent/plans/20260928-billing-address-and-diagnostics.md --profile critical`,
check local links and task dependencies, and review the producer/consumer contract.
The installed Jig here has no `file-budget` command.

## Rollout and recovery

Provide the tested exact revision (descending from `16801e5`), synchronized 0.5.4
versions, v5 SQL and cutover README to IdentityPro. Before any host upgrade,
prebuild its v5-aware API and worker, rehearse on a restored production-sized copy,
measure locks and duration, and stop all writers during the approved maintenance
window. Old v4 binaries must not restart after DDL commit; already-running v4
processes keep writing (the columns are nullable), so the writer stop is what
enforces the cutover. Forward recovery uses a v5-aware build; restoring an old image
or erasing new columns is not a rollback. Do not hold transactions across NMI I/O. A
later sandbox enrollment/query can verify provider round-trip address presence on
enrollment; renewal transport is proven by wire tests. Production sales are outside
this plan's authorization; AVS need not match in sandbox.

## Risks and open decisions

- NMI processor acceptance is outside our control; the host/operator owns the
  TSYS/MCC investigation. Address and diagnostic delivery do not depend on it.
- Existing subscribers stay addressless until they complete a supported
  customer-present operation (IdentityPro card replacement is unfinished), so
  this release changes nothing for current renewals, including the incident's.
- Owner: release manager. Fast-forward `release/0.5.4` (an ancestor of the pin) or
  name another work branch,
  merge `release/0.5.3` records before release, and record the 0.5.x exception.
  Trigger: before freezing the producer revision.
- Owner: mainline maintainers. Renumber unreleased mainline schemas above this v5
  and add an upgrade from it before IdentityPro moves to 0.6.
- Owner: T-02 implementer. Prove the lock order with deterministic tests; do not
  add an inverted lock or weaken replacement fences.
- Diagnostic load can delay renewals through the shared cooldown (D-04); the host
  bounds refresh volume. Query action ambiguity is contained by `Unavailable`, not
  by choosing a convenient action.
- The v5 CHECK validation scans both tables under an exclusive lock; the host
  rehearsal decides whether the one-transaction upgrade fits the window.
- Host card-replacement UI is tracked separately in IdentityPro; producer support
  must be available without declaring that UI delivered.

### Planning validation and tracker handoff — 2026-09-28

Delivery epic: `syrup-rail-e7h`. All tasks remain open and unclaimed.

| Plan task | Bead |
| --- | --- |
| T-01 | `syrup-rail-e7h.1` |
| T-02 | `syrup-rail-e7h.2` |
| T-03 | `syrup-rail-e7h.3` |

The first draft was committed on `integration/0.5.3` (`304605b`); this revision
supersedes it on the existing pin-based work branch. Use this revised plan when
starting T-01; the plan committed on
`integration/0.5.3` is not the current execution contract. Ready delivery roots are
`syrup-rail-e7h.1` and `.3`; start with `.1`. Consumer adoption
`identitypro-y0c5.1` is blocked on `external:syrup-rail:syrup-rail-e7h`. After
verified delivery, record the exact tested revision/version in that consumer bead
before removing its external edge.

Revision note (2026-09-28): corrected the NMI source and numeric limits using the
actual sale/validate request schemas, aligned consumer pre-validation, and added
boundary coverage without claiming provider acceptance. Refreshed the restart
checkpoint to the existing work branch. Only the plan changed; implementation and
provider behavior remain unverified.
