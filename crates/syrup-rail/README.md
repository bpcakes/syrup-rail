# syrup-rail

`syrup-rail` contains the provider-independent domain types and lifecycle
policy for Syrup Rail billing. It validates subscription terms, snapshots paid
trial and recurring authority, models dunning and terminal nonpayment, exposes
canonical entitlement decisions, and defines the gateway and host-admission
ports used by the adapter crates.

```toml
[dependencies]
syrup-rail = "0.7.0"
```

Construct subscription offers from explicit start, recurring, and failure
terms. Dunning delays are relative to the preceding submitted determinate
automatic-renewal failure:

```rust
use syrup_rail::{
    ChargeAmount, CurrencyCode, DunningExhaustion, DunningRetryDelay,
    DunningSchedule, PastDueAccessPolicy, PlanKey, RecurringSubscriptionTerms,
    RenewalFailurePolicy, SubscriptionOffer, SubscriptionPeriodRule,
    SubscriptionStart,
};

let usd = CurrencyCode::new("USD")?;
let offer = SubscriptionOffer::new(
    PlanKey::new("base_subscription")?,
    RecurringSubscriptionTerms::new(
        ChargeAmount::new(1_000, usd)?,
        SubscriptionPeriodRule::calendar_months(1)?,
    ),
    SubscriptionStart::RecurringImmediately,
    RenewalFailurePolicy::new(
        DunningSchedule::from_delays([
            DunningRetryDelay::days(1)?,
            DunningRetryDelay::days(3)?,
        ])?,
        DunningExhaustion::MarkUnpaid,
        PastDueAccessPolicy::ContinueUntilDunningExhausted,
    ),
)?;
# let _ = offer;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `Entitlement::permits_product_access()` for the canonical product-access
decision. A `past_due` payment state does not by itself imply suspended access.
For `BillingEvent::SubscriptionPaymentFailed`, consume the event's typed
`access` outcome rather than rebuilding it from current offer configuration.

Use `PaymentCardBrand` for customer-facing or host-event card presentation.
`PaymentCardBrand::from_provider` maps recognized provider values into a
stable vocabulary and reduces unknown nonempty text to `Other`, so arbitrary
provider strings do not cross that projection boundary. Its aliases include
the documented NMI `diners` label for `DinersClub`; provider adapters retain
exact evidence and test their own documented scheme vocabulary.

`SubscriptionPaymentMethodDisplay` cannot represent an all-empty display.
Its persisted/provider-parts conversion normalizes first and returns `None`
when no renderable field remains.

`PaymentGateway::query_payment_method_metadata` returns a
`GatewayPaymentMethodMetadata` observation for an already approved saved method.
The default implementation projects an ordinary exact query, so existing gateway
implementations continue to work. Providers may override it to enrich display
without changing financial query evidence. The metadata result retains identity,
status and diagnostics for rejection checks. The API provides no conversion to
`ProcessorEvidence` and discards financial response fields; hosts must not
reconstruct a financial approval from a display observation.
Gateway decorators must forward `query_payment_method_metadata` to the wrapped
gateway as well as the five required methods. Otherwise, the default projects
the decorator's financial query and can omit the provider's enriched fields.

A `BillingContact` can carry a customer-confirmed `BillingAddress`: attach one
with `with_address`, or build an address-only contact with `from_address`.
The address requires a first line and a two-letter country, trims every field,
limits each field to 255 bytes, rejects control characters, stores the country
uppercase, and keeps postal codes as text. It is country-neutral. The address
is part of the durable `BillingContactSnapshot`, so a same-key retry with an
added, removed, or changed address is a different request; attempt fingerprints
never include contacts. `BillingContact::into_parts` returns
`BillingContactParts`, which includes the address. Provider adapters can apply
stricter formats. An address the adapter rejects fails before submission after
the attempt has been reserved, and a corrected retry needs a new idempotency
key, so hosts must pre-validate to their adapter's documented rules. Local
validation never proves that a provider or processor accepts the address.

A `StatementDescriptor` is the merchant name (at most 25 printable-ASCII
bytes) with an optional customer-service phone (20 bytes) and URL (40 bytes)
that a provider may print on the cardholder's statement for one sale where the
acquirer supports dynamic descriptors. A host builds a
`StatementDescriptorPolicy` from a default descriptor and, optionally, the
wording for the first recurring charge after a paid trial, and attaches it to
its `ResolvedGateway` with `with_statement_descriptors`; `ResolvedGateway::sale`
then fills `GatewaySaleRequest::descriptor` by intent unless the request
already carries one. `GatewaySaleIntent::RecurringStoredCredential` names
that first post-trial charge with `first_charge_after_paid_trial`, which the
PostgreSQL renewal workflow sets from the subscription phase, and
`GatewaySaleRequest::into_parts` returns `GatewaySaleRequestParts`. A decorator
can rebuild the request losslessly with `GatewaySaleRequest::from(parts)`;
when rebuilding with `GatewaySaleRequest::new`, it must also forward
`parts.descriptor` with `with_descriptor`. The limits are transport
bounds: the networks print the name in a 25- or 22-character field that the
acquirer's prefix shares and the phone or URL in 13 characters, and only a
real statement proves what printed.

A `CardholderIpAddress` is the network address the cardholder used when they
personally authorized a payment, as the host established it. A host attaches it
with `SubscriptionPaymentContext::with_cardholder_ip`, and enrollment and
recovery forward it on their token sale as `GatewaySaleRequest::cardholder_ip`.
It is memory-only: Syrup Rail never stores it, never compares it on replay, and
makes no payment or entitlement decision from it, and its `Debug` and `Display`
output is value-free. Provider text that repeats it is stored as provider text,
though card-number redaction may remove some or all of it. The host vouches
that the address is the cardholder's own, not its server's or a
representative's, and owns the disclosure that it is shared with the provider.
A decorator that rebuilds a sale with `GatewaySaleRequest::new` must forward
`parts.cardholder_ip` with `with_cardholder_ip`.

`PaymentGateway::query_transaction_diagnostics` reads processor and verification
results for one historical transaction. A `GatewayTransactionDiagnosticsRequest`
names the transaction ID, the original operation (`Sale` or `Validate`), its
amount, and the expected order ID; providers must look up only that transaction
and select exactly one matching original action. Results are observations, not
payment evidence: `GatewayTransactionDiagnosticsObservation` omits provider codes
longer than 64 bytes as received or after sanitization (and limits AVS codes to
one ASCII letter or digit via `GatewayAvsResponse`), sanitizes free text, and
reports partial completeness when a field is missing. The default returns `Unsupported` without I/O, and gateway
decorators must forward the method.

`GatewayAvsResponse` represents a provider's single-character ASCII AVS result
code (`parse`, `as_char`, `as_str`) with value-free `Debug` and `Display`
formatting; any other value parses to `None` rather than being shortened.
`ProcessorEvidence::with_avs_response` / `avs_response` and
`GatewayPaymentOutcome::avs_response` carry it alongside a payment response as
a diagnostic without changing payment decisions.

Syrup Rail does not authenticate subscribers, persist gateway credentials, or
define a transport wire format for billing events. Those remain host
responsibilities. See the workspace `subscription_terms`, `entitlement_access`,
and PostgreSQL `host_integration` examples for complete integration patterns.

This package is proprietary software distributed under the terms in the
packaged `LICENSE` file.
