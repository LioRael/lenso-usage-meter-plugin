CREATE TABLE usage_aggregates (
    scope_kind text NOT NULL,
    scope_id text NOT NULL,
    subject text NOT NULL,
    meter_key text NOT NULL,
    aggregate_revision bigint NOT NULL DEFAULT 0 CHECK (aggregate_revision >= 0),
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    PRIMARY KEY (scope_kind, scope_id, subject, meter_key)
);

CREATE TABLE usage_entries (
    entry_id text PRIMARY KEY,
    entry_kind text NOT NULL CHECK (entry_kind IN ('event', 'correction')),
    original_event_id text REFERENCES usage_entries(entry_id),
    scope_kind text NOT NULL,
    scope_id text NOT NULL,
    subject text NOT NULL,
    meter_key text NOT NULL,
    quantity bigint NOT NULL CHECK (quantity <> 0),
    occurred_at timestamptz NOT NULL,
    reason text,
    created_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    FOREIGN KEY (scope_kind, scope_id, subject, meter_key)
        REFERENCES usage_aggregates(scope_kind, scope_id, subject, meter_key),
    CHECK (
        (entry_kind = 'event' AND original_event_id IS NULL AND reason IS NULL AND quantity > 0)
        OR
        (entry_kind = 'correction' AND original_event_id IS NOT NULL AND reason IS NOT NULL)
    )
);

CREATE INDEX usage_entries_window_idx
    ON usage_entries(scope_kind, scope_id, subject, meter_key, occurred_at);

CREATE INDEX usage_entries_original_idx
    ON usage_entries(original_event_id)
    WHERE original_event_id IS NOT NULL;
