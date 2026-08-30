# Lenso Usage Meter Plugin

This repository contains the removable vNext Usage Meter deletion boundary. It
records immutable usage events, append-only corrections, and exact aggregate
windows through `lenso.usage-meter@1`.

Reader, producer, and correction callers are admitted explicitly. Event and
correction IDs are idempotency keys: an exact replay is a no-op, while a
different payload for the same ID is rejected. Every accepted append advances
the affected aggregate revision in the same transaction. Event and window
timestamps must resolve to whole microseconds so PostgreSQL round trips preserve
exact replay and boundary semantics.

Each event and correction quantity is an int64, while a window total is an
exact arbitrary-precision decimal integer string. The accumulator therefore
does not silently overflow when many individually valid entries are summed.

The Plugin does not read or mutate Entitlements. Billing and business Plugins
choose when to record usage and how to interpret the returned aggregate fact.

## Distribution boundary

`lenso-capability-usage-meter` is the public, runtime-independent collaboration
contract. `lenso-usage-meter-postgres-plugin` remains a repository-local linked
implementation: it is tested here but is not published to crates.io. This keeps
consumers on the Capability instead of coupling them to the Plugin's private
PostgreSQL schema or implementation types.

See [the release process](docs/release-process.md) for the review, package, and
Trusted Publishing gates.
