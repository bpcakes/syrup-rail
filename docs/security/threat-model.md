# Syrup Rail security and privacy threat model

This document is the security and privacy baseline for the reusable Syrup Rail
crates. It describes the guarantees these packages may claim and the boundaries
that remain owned by each host application.

## System boundary

Syrup Rail contains four private Rust packages:

- `syrup-rail` owns validated billing identities, lifecycle policy, gateway
  contracts, and command outcomes.
- `syrup-rail-postgres` owns the provider-neutral financial ledger and
  transaction orchestration on a host-supplied PostgreSQL connection.
- `syrup-rail-nmi` maps the domain gateway contract to NMI.
- `syrup-rail-nmi-client` owns bounded, retry-free NMI transport and parsing.

Hosts own authentication, authorization, pricing/catalog rows, encryption and
rotation of stored provider credentials, abuse controls, user deletion,
application-specific jobs and events, and presentation. A host must authorize
the billing scope and subscriber before calling a shared command.

## Protected data

- Provider API and query credentials while a host-resolved gateway client is
  alive.
- Browser-generated payment tokens and provider payment-method references.
- Transaction identifiers, processor decisions, refunds, voids, chargebacks,
  and operator attestations.
- Subscription periods, amounts, discounts, grants, and durable attempt state.
- Billing contact and card-display metadata retained only where required for a
  command or stored-method display.
- The cardholder network address a host may attach to a customer-initiated
  token sale. These crates hold it only in the host's command and the request
  built from it, and for as long as it takes to parse a later provider query
  response that contains it. They never extract it from such a response, never
  persist or log it as a field, and redact it from `Debug` and `Display` output.
  The provider receives and records it, and may repeat it in text these crates
  persist; see
  [Cardholder network address transfer](#cardholder-network-address-transfer).

PAN and CVV are not accepted or stored by these crates. Hosts must tokenize card
data in the browser through the approved provider boundary.

## Attackers and trust boundaries

The in-scope attacker may control public command fields, idempotency keys,
provider responses, report XML/JSON/form bodies, timing and cancellation of
requests, and repeated or concurrent requests. The model also includes an
authenticated subscriber trying to cross a billing scope or plan boundary and
an operator making a mistaken but otherwise authorized reconciliation action.

The relevant boundaries are:

1. Host authorization and pricing policy into the provider-neutral command API.
2. Host credential storage into a short-lived provider client.
3. Shared command orchestration into the canonical PostgreSQL ledger.
4. NMI transport into untrusted provider response parsing.
5. Durable processor evidence into host entitlement and fulfillment effects.
6. Operator review into immutable attestation and quarantine transitions.
7. Host account deletion into retained financial-subject and PII-scrub policy.

## Required controls

- A mutation is submitted at most once by the raw client. Indeterminate outcomes
  are reconciled from durable attempt identity; they are never blindly retried.
- Durable idempotency and current-state admission are checked under the documented
  PostgreSQL transaction and lock order before provider I/O or state projection.
- Approved processor evidence is retained even when application-state projection
  fails, and conflicting transaction ownership fails closed.
- Provider text and identifiers are untrusted. Ordinary formatting and errors are
  value-free; callers explicitly expose, validate, and minimize values before
  persistence or logging.
- Credentials use directly controlled zeroizing buffers and are never installed
  as shared default headers. This reduces accidental lifetime and disclosure but
  is not a memory-forensics guarantee.
- Request fields, encoded bodies, report bodies, parsed report cardinality, and
  concurrent report work are bounded before untrusted input can consume
  unbounded crate-owned resources.
- Canonical tables contain no host authentication or authorization policy.
  Host callbacks use the same supplied SQL transaction and cannot open a second
  connection to bypass lock order or atomicity.
- Financial deletion behavior is explicit in each host. No host may attach
  `ON DELETE CASCADE` from a live user row to the canonical financial ledger.

## Explicit non-protections

These controls do not protect against a compromised application process, memory
forensics, a root-equivalent operator, a malicious database superuser, or an
operator with unrestricted direct SQL access. They do not make provider claims
truthful, provide card-network dispute adjudication, or replace host-side
authentication, authorization, rate limiting, credential encryption, backups,
retention schedules, or incident response.

Zeroization cannot erase copies made by HTTP, JSON, TLS, allocators, the
operating system, or callers after explicit exposure. PostgreSQL integrity does
not protect a host that runs unreviewed DDL with superuser privileges.

## Cardholder network address transfer

Sending a cardholder's network address to NMI is an external data transfer, so
it answers the review test below.

1. **Protected data and attacker.** The address is personal data. The controls
   address a reader of logs, errors, or the database, and an ordinary coding
   mistake that would send it with a charge no cardholder took part in.
2. **Trust boundary and prevented attack.** The address crosses the host command
   API into NMI transport. It is opt-in per request, the raw client keeps it off
   the form `Debug` allowlist, and the NMI adapter drops it from every
   merchant-initiated sale whatever the request holds.
3. **Simpler controls considered.** Sending nothing is the default. It is not
   neutral at the provider: for a sale without the field NMI's sandbox recorded
   the submitting server's address. Redaction and bounding reuse the existing
   form controls; no new cryptography or storage is added.
4. **Lifecycle and operational cost.** There is no key. The value lives as long as
   the host's command and its clones, is not zeroized, and is copied into the
   encoded request body. NMI keeps it with the transaction under its own
   retention and documents an IP address field in its query responses, so a
   later query can bring it back into memory as response text. The cost falls on
   the host: taking the address from trusted ingress, disclosing the transfer,
   and forwarding the address in any gateway decorator that rebuilds a sale.
5. **Explicit non-protections.** Syrup Rail does not check that the address is
   routable or belongs to the cardholder; a host must take it from trusted
   ingress, and a VPN, proxy, or assisted signup can still hide the cardholder.
   Scrubbing subscriber billing data cannot recall it from the provider. If the
   provider repeats the address in response or error text, the sanitizer does
   not recognize it, so it is persisted as provider text, though card-number
   redaction may remove some or all of it, and digits beside it; copies in
   immutable charge evidence, external-reversal attestations, and attempt
   fields the scrub does not clear outlive it. NMI's gateway accepted and
   recorded IPv4 and IPv6 in its sandbox; what a processor does with either is
   unverified, and a merchant account's IP rules may approve or decline on it.
   The host owns its disclosure.

## Review test for a new security mechanism

Before adding or materially expanding encryption, hashing/HMAC, signatures,
zeroization, sensitive persistence, external data transfer, privileged
operations, or tamper-evident machinery, document:

1. the protected data and attacker;
2. the trust boundary and prevented attack;
3. simpler existing controls considered;
4. key/data lifecycle and operational cost; and
5. explicit non-protections.

Do not add a mechanism that merely duplicates an existing control or claims to
address an attacker excluded above without an explicitly reviewed stronger
feature-specific model.
