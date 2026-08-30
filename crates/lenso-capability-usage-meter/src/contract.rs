//! Authoritative source for the Usage Meter Capability contract.

use lenso_contract_authoring as lenso;

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct RecordUsageRequest {
    pub event_id: String,
    pub scope_kind: String,
    pub scope_id: String,
    pub subject: String,
    pub meter: String,
    /// Positive base-10 int64 encoded as a portable string.
    pub quantity: String,
    /// RFC 3339 timestamp that resolves to a whole microsecond.
    #[schemars(extend("format" = "date-time"))]
    pub occurred_at: String,
}

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct RecordUsageResponse {
    pub accepted: bool,
    #[schemars(range(min = 0))]
    pub aggregate_revision: i64,
}

#[derive(lenso::DomainError)]
pub enum RecordUsageError {
    InvalidEvent,
    IdempotencyConflict,
    Forbidden,
}

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct CorrectUsageRequest {
    pub correction_id: String,
    pub original_event_id: String,
    /// Non-zero signed base-10 int64 encoded as a portable string.
    pub quantity_delta: String,
    pub reason: String,
}

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct CorrectUsageResponse {
    pub accepted: bool,
    #[schemars(range(min = 0))]
    pub aggregate_revision: i64,
}

#[derive(lenso::DomainError)]
pub enum CorrectUsageError {
    InvalidCorrection,
    OriginalNotFound,
    IdempotencyConflict,
    Forbidden,
}

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ReadUsageWindowRequest {
    pub scope_kind: String,
    pub scope_id: String,
    pub subject: String,
    pub meter: String,
    /// RFC 3339 timestamp that resolves to a whole microsecond.
    #[schemars(extend("format" = "date-time"))]
    pub window_start: String,
    /// RFC 3339 timestamp that resolves to a whole microsecond.
    #[schemars(extend("format" = "date-time"))]
    pub window_end: String,
}

#[derive(lenso::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ReadUsageWindowResponse {
    /// Signed arbitrary-precision base-10 integer encoded as a portable string.
    ///
    /// Each individual event and correction is int64, but their exact sum may
    /// exceed int64.
    pub quantity: String,
    #[schemars(range(min = 0))]
    pub event_count: i64,
    #[schemars(range(min = 0))]
    pub aggregate_revision: i64,
}

#[derive(lenso::DomainError)]
pub enum ReadUsageWindowError {
    InvalidWindow,
    Forbidden,
}

#[lenso::capability(
    id = "lenso.usage-meter",
    major = 1,
    version = "1.0.0",
    portable = true,
    cross_lane_transfer = true
)]
pub trait UsageMeter {
    async fn record_usage(
        &self,
        context: lenso::Ctx<'_>,
        request: RecordUsageRequest,
    ) -> Result<RecordUsageResponse, RecordUsageError>;

    async fn correct_usage(
        &self,
        context: lenso::Ctx<'_>,
        request: CorrectUsageRequest,
    ) -> Result<CorrectUsageResponse, CorrectUsageError>;

    async fn read_usage_window(
        &self,
        context: lenso::Ctx<'_>,
        request: ReadUsageWindowRequest,
    ) -> Result<ReadUsageWindowResponse, ReadUsageWindowError>;
}
