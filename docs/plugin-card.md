# vNext Usage Meter Plugin card

## Owner and deletion boundary

`lenso-usage-meter-postgres-plugin` owns append-only usage entries, corrections,
aggregate windows, and per-aggregate revisions in a private PostgreSQL schema.

## Roles and authority

- Provides `lenso.usage-meter@1`.
- Exact reader callers may read aggregate facts; the target remains final
  authority.
- Exact producer callers may record positive events.
- Exact administrator callers may append non-zero corrections.
- Requires `lenso.secrets@1` only for the owned database URL.

## First observable behavior

A producer records a stable event ID. Exact retries do not double count and a
payload conflict fails closed. Administrators correct an original event with a
new immutable ID. Window reads sum originals and corrections without consulting
Entitlements, plans, invoices, or Access Control. Individual entries are int64;
the returned exact sum is an arbitrary-precision decimal integer string.
