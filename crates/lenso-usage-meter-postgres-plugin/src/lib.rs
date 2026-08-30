//! Append-only, idempotent usage events and corrections.

mod operator;
mod schema;

use std::{cell::RefCell, fmt, rc::Rc, time::Duration};

use lenso::{ActivateContext, DeactivateContext, Lifecycle, Port, provides};
use lenso_capability_secrets as secrets;
use lenso_capability_secrets::{ResolveRequest, SecretsClient, SecretsInvocationError};
use lenso_capability_usage_meter as usage;
use lenso_capability_usage_meter::{
    CorrectUsageError, CorrectUsageRequest, CorrectUsageResponse, ReadUsageWindowError,
    ReadUsageWindowRequest, ReadUsageWindowResponse, RecordUsageError, RecordUsageRequest,
    RecordUsageResponse, UsageMeterCorrectUsage, UsageMeterReadUsageWindow, UsageMeterRecordUsage,
};
use lenso_kernel::{InvocationContext, NativeRequestFuture, RuntimeFailure};
use lenso_postgres_kit::OwnedPostgres;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroizing;

use crate::schema::schema_plan;

pub use operator::{UsageMeterOperator, UsageMeterOperatorError};

const DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageMeterConfig {
    schema: String,
    database_url_secret: String,
    reader_callers: Vec<String>,
    producer_callers: Vec<String>,
    admin_callers: Vec<String>,
}

impl UsageMeterConfig {
    pub fn new(
        schema: impl Into<String>,
        database_url_secret: impl Into<String>,
        reader_callers: Vec<String>,
        producer_callers: Vec<String>,
        admin_callers: Vec<String>,
    ) -> Result<Self, UsageMeterConfigError> {
        let value = Self {
            schema: schema.into(),
            database_url_secret: database_url_secret.into(),
            reader_callers,
            producer_callers,
            admin_callers,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), UsageMeterConfigError> {
        schema_plan(self.schema.clone()).map_err(|_| UsageMeterConfigError::InvalidSchema)?;
        if !valid_secret_reference(&self.database_url_secret) {
            return Err(UsageMeterConfigError::InvalidSecretReference);
        }
        if self.reader_callers.is_empty()
            || self.reader_callers.iter().any(|caller| !valid_name(caller))
        {
            return Err(UsageMeterConfigError::InvalidReaderCallers);
        }
        if self.producer_callers.is_empty()
            || self
                .producer_callers
                .iter()
                .any(|caller| !valid_name(caller))
        {
            return Err(UsageMeterConfigError::InvalidProducerCallers);
        }
        if self.admin_callers.is_empty()
            || self.admin_callers.iter().any(|caller| !valid_name(caller))
        {
            return Err(UsageMeterConfigError::InvalidAdminCallers);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum UsageMeterConfigError {
    #[error("invalid owned PostgreSQL schema")]
    InvalidSchema,
    #[error("invalid database URL secret reference")]
    InvalidSecretReference,
    #[error("at least one valid aggregate reader is required")]
    InvalidReaderCallers,
    #[error("at least one valid producer caller is required")]
    InvalidProducerCallers,
    #[error("at least one valid correction administrator is required")]
    InvalidAdminCallers,
}

fn validate_config(config: &UsageMeterConfig) -> Result<(), RuntimeFailure> {
    config
        .validate()
        .map_err(|error| RuntimeFailure::InvalidResolvedPlan {
            detail: error.to_string(),
        })
}

#[lenso::plugin(
    lifecycle,
    configuration_schema = "configuration.schema.json",
    validate = validate_config
)]
#[derive(Clone)]
struct UsageMeterPlugin {
    #[config]
    config: UsageMeterConfig,
    secrets: Port<secrets::SecretsClient>,
    state: Rc<RefCell<Option<PreparedUsageMeter>>>,
}

#[derive(Clone)]
struct PreparedUsageMeter {
    postgres: OwnedPostgres,
}

impl fmt::Debug for PreparedUsageMeter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedUsageMeter")
            .field("schema", &self.postgres.schema())
            .finish()
    }
}

impl fmt::Debug for UsageMeterPlugin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsageMeterPlugin")
            .field("prepared", &self.state.borrow().is_some())
            .field("reader_caller_count", &self.config.reader_callers.len())
            .field("producer_caller_count", &self.config.producer_callers.len())
            .field("admin_caller_count", &self.config.admin_callers.len())
            .finish_non_exhaustive()
    }
}

#[provides(usage::UsageMeter)]
impl UsageMeterPlugin {}

impl UsageMeterPlugin {
    fn prepared(&self) -> Result<PreparedUsageMeter, RuntimeFailure> {
        self.state
            .borrow()
            .clone()
            .ok_or(RuntimeFailure::PluginFailure {
                detail: "Usage Meter Plugin is not prepared".to_owned(),
            })
    }

    fn producer_authorized(&self, context: &InvocationContext) -> bool {
        caller_allowed(context, &self.config.producer_callers)
    }

    fn reader_authorized(&self, context: &InvocationContext) -> bool {
        caller_allowed(context, &self.config.reader_callers)
    }

    fn admin_authorized(&self, context: &InvocationContext) -> bool {
        caller_allowed(context, &self.config.admin_callers)
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    fn record_usage(
        &self,
        context: InvocationContext,
        request: RecordUsageRequest,
    ) -> NativeRequestFuture<UsageMeterRecordUsage> {
        let authorized = self.producer_authorized(&context);
        let prepared = self.prepared();
        Box::pin(async move {
            if !authorized {
                return Ok(Err(RecordUsageError::Forbidden));
            }
            let quantity = request.quantity.parse::<i64>().ok();
            let occurred_at = OffsetDateTime::parse(&request.occurred_at, &Rfc3339)
                .ok()
                .filter(pg_timestamp_representable);
            if !valid_name(&request.event_id)
                || !valid_dimension(&request.scope_kind)
                || !valid_name(&request.scope_id)
                || !valid_name(&request.subject)
                || !valid_dimension(&request.meter)
                || quantity.is_none_or(|value| value <= 0)
                || occurred_at.is_none()
            {
                return Ok(Err(RecordUsageError::InvalidEvent));
            }
            let quantity = quantity.expect("validated");
            let occurred_at = occurred_at.expect("validated");
            let prepared = prepared?;
            let mut transaction = prepared.postgres.pool().begin().await.map_err(|source| {
                runtime(UsageMeterError::Database {
                    operation: "begin usage event",
                    source,
                })
            })?;
            ensure_aggregate(
                &mut transaction,
                &request.scope_kind,
                &request.scope_id,
                &request.subject,
                &request.meter,
            )
            .await?;
            let revision = lock_revision(
                &mut transaction,
                &request.scope_kind,
                &request.scope_id,
                &request.subject,
                &request.meter,
            )
            .await?;
            let existing = sqlx::query("SELECT entry_kind,scope_kind,scope_id,subject,meter_key,quantity,occurred_at FROM usage_entries WHERE entry_id=$1")
                .bind(&request.event_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|source| runtime(UsageMeterError::Database { operation: "read usage idempotency key", source }))?;
            if let Some(row) = existing {
                let entry_kind: String = decode(&row, "entry_kind", "decode usage entry kind")?;
                let scope_kind: String = decode(&row, "scope_kind", "decode usage scope kind")?;
                let scope_id: String = decode(&row, "scope_id", "decode usage scope id")?;
                let subject: String = decode(&row, "subject", "decode usage subject")?;
                let meter: String = decode(&row, "meter_key", "decode usage meter")?;
                let stored_quantity: i64 = decode(&row, "quantity", "decode usage quantity")?;
                let stored_occurrence: OffsetDateTime =
                    decode(&row, "occurred_at", "decode usage occurrence")?;
                let same = entry_kind == "event"
                    && scope_kind == request.scope_kind
                    && scope_id == request.scope_id
                    && subject == request.subject
                    && meter == request.meter
                    && stored_quantity == quantity
                    && stored_occurrence == occurred_at;
                if !same {
                    return Ok(Err(RecordUsageError::IdempotencyConflict));
                }
                transaction.commit().await.map_err(|source| {
                    runtime(UsageMeterError::Database {
                        operation: "commit repeated usage event",
                        source,
                    })
                })?;
                return Ok(Ok(RecordUsageResponse {
                    accepted: false,
                    aggregate_revision: revision,
                }));
            }
            let inserted = sqlx::query("INSERT INTO usage_entries(entry_id,entry_kind,scope_kind,scope_id,subject,meter_key,quantity,occurred_at) VALUES($1,'event',$2,$3,$4,$5,$6,$7)")
                .bind(&request.event_id)
                .bind(&request.scope_kind)
                .bind(&request.scope_id)
                .bind(&request.subject)
                .bind(&request.meter)
                .bind(quantity)
                .bind(occurred_at)
                .execute(&mut *transaction)
                .await;
            if let Err(source) = inserted {
                if unique_violation(&source) {
                    return Ok(Err(RecordUsageError::IdempotencyConflict));
                }
                return Err(runtime(UsageMeterError::Database {
                    operation: "insert usage event",
                    source,
                }));
            }
            let aggregate_revision = advance_revision(
                &mut transaction,
                &request.scope_kind,
                &request.scope_id,
                &request.subject,
                &request.meter,
            )
            .await?;
            transaction.commit().await.map_err(|source| {
                runtime(UsageMeterError::Database {
                    operation: "commit usage event",
                    source,
                })
            })?;
            Ok(Ok(RecordUsageResponse {
                accepted: true,
                aggregate_revision,
            }))
        })
    }

    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    fn correct_usage(
        &self,
        context: InvocationContext,
        request: CorrectUsageRequest,
    ) -> NativeRequestFuture<UsageMeterCorrectUsage> {
        let authorized = self.admin_authorized(&context);
        let prepared = self.prepared();
        Box::pin(async move {
            if !authorized {
                return Ok(Err(CorrectUsageError::Forbidden));
            }
            let quantity = request.quantity_delta.parse::<i64>().ok();
            if !valid_name(&request.correction_id)
                || !valid_name(&request.original_event_id)
                || quantity.is_none_or(|value| value == 0)
                || request.reason.trim().is_empty()
                || request.reason.len() > 512
                || request.reason.contains('\0')
            {
                return Ok(Err(CorrectUsageError::InvalidCorrection));
            }
            let quantity = quantity.expect("validated");
            let prepared = prepared?;
            let mut transaction = prepared.postgres.pool().begin().await.map_err(|source| {
                runtime(UsageMeterError::Database {
                    operation: "begin usage correction",
                    source,
                })
            })?;
            let original = sqlx::query("SELECT scope_kind,scope_id,subject,meter_key,occurred_at FROM usage_entries WHERE entry_id=$1 AND entry_kind='event'")
                .bind(&request.original_event_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|source| runtime(UsageMeterError::Database { operation: "read original usage event", source }))?;
            let Some(original) = original else {
                return Ok(Err(CorrectUsageError::OriginalNotFound));
            };
            let scope_kind: String = decode(&original, "scope_kind", "decode usage scope kind")?;
            let scope_id: String = decode(&original, "scope_id", "decode usage scope id")?;
            let subject: String = decode(&original, "subject", "decode usage subject")?;
            let meter: String = decode(&original, "meter_key", "decode usage meter")?;
            let occurred_at: OffsetDateTime =
                decode(&original, "occurred_at", "decode usage occurrence")?;
            let revision =
                lock_revision(&mut transaction, &scope_kind, &scope_id, &subject, &meter).await?;
            let existing = sqlx::query("SELECT entry_kind,original_event_id,quantity,reason FROM usage_entries WHERE entry_id=$1")
                .bind(&request.correction_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|source| runtime(UsageMeterError::Database { operation: "read correction idempotency key", source }))?;
            if let Some(row) = existing {
                let entry_kind: String =
                    decode(&row, "entry_kind", "decode correction entry kind")?;
                let original_event_id: Option<String> = decode(
                    &row,
                    "original_event_id",
                    "decode correction original event",
                )?;
                let stored_quantity: i64 = decode(&row, "quantity", "decode correction quantity")?;
                let reason: Option<String> = decode(&row, "reason", "decode correction reason")?;
                let same = entry_kind == "correction"
                    && original_event_id.as_deref() == Some(request.original_event_id.as_str())
                    && stored_quantity == quantity
                    && reason.as_deref() == Some(request.reason.as_str());
                if !same {
                    return Ok(Err(CorrectUsageError::IdempotencyConflict));
                }
                transaction.commit().await.map_err(|source| {
                    runtime(UsageMeterError::Database {
                        operation: "commit repeated usage correction",
                        source,
                    })
                })?;
                return Ok(Ok(CorrectUsageResponse {
                    accepted: false,
                    aggregate_revision: revision,
                }));
            }
            let inserted = sqlx::query("INSERT INTO usage_entries(entry_id,entry_kind,original_event_id,scope_kind,scope_id,subject,meter_key,quantity,occurred_at,reason) VALUES($1,'correction',$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(&request.correction_id)
                .bind(&request.original_event_id)
                .bind(&scope_kind)
                .bind(&scope_id)
                .bind(&subject)
                .bind(&meter)
                .bind(quantity)
                .bind(occurred_at)
                .bind(request.reason)
                .execute(&mut *transaction)
                .await;
            if let Err(source) = inserted {
                if unique_violation(&source) {
                    return Ok(Err(CorrectUsageError::IdempotencyConflict));
                }
                return Err(runtime(UsageMeterError::Database {
                    operation: "insert usage correction",
                    source,
                }));
            }
            let aggregate_revision =
                advance_revision(&mut transaction, &scope_kind, &scope_id, &subject, &meter)
                    .await?;
            transaction.commit().await.map_err(|source| {
                runtime(UsageMeterError::Database {
                    operation: "commit usage correction",
                    source,
                })
            })?;
            Ok(Ok(CorrectUsageResponse {
                accepted: true,
                aggregate_revision,
            }))
        })
    }

    #[allow(clippy::needless_pass_by_value)]
    fn read_usage_window(
        &self,
        context: InvocationContext,
        request: ReadUsageWindowRequest,
    ) -> NativeRequestFuture<UsageMeterReadUsageWindow> {
        let authorized = self.reader_authorized(&context);
        let prepared = self.prepared();
        Box::pin(async move {
            if !authorized {
                return Ok(Err(ReadUsageWindowError::Forbidden));
            }
            let window_start = OffsetDateTime::parse(&request.window_start, &Rfc3339)
                .ok()
                .filter(pg_timestamp_representable);
            let window_end = OffsetDateTime::parse(&request.window_end, &Rfc3339)
                .ok()
                .filter(pg_timestamp_representable);
            if !valid_dimension(&request.scope_kind)
                || !valid_name(&request.scope_id)
                || !valid_name(&request.subject)
                || !valid_dimension(&request.meter)
                || window_start
                    .zip(window_end)
                    .is_none_or(|(start, end)| start >= end)
            {
                return Ok(Err(ReadUsageWindowError::InvalidWindow));
            }
            let prepared = prepared?;
            let start = window_start.expect("validated");
            let end = window_end.expect("validated");
            let row = sqlx::query("SELECT COALESCE(SUM(quantity),0)::text AS quantity,COUNT(*) FILTER (WHERE entry_kind='event')::bigint AS event_count,COALESCE((SELECT aggregate_revision FROM usage_aggregates WHERE scope_kind=$1 AND scope_id=$2 AND subject=$3 AND meter_key=$4),0)::bigint AS aggregate_revision FROM usage_entries WHERE scope_kind=$1 AND scope_id=$2 AND subject=$3 AND meter_key=$4 AND occurred_at >= $5 AND occurred_at < $6")
                .bind(&request.scope_kind)
                .bind(&request.scope_id)
                .bind(&request.subject)
                .bind(&request.meter)
                .bind(start)
                .bind(end)
                .fetch_one(prepared.postgres.pool())
                .await
                .map_err(|source| runtime(UsageMeterError::Database { operation: "read usage window", source }))?;
            Ok(Ok(ReadUsageWindowResponse {
                quantity: decode(&row, "quantity", "decode usage quantity")?,
                event_count: decode(&row, "event_count", "decode usage event count")?,
                aggregate_revision: decode(&row, "aggregate_revision", "decode usage revision")?,
            }))
        })
    }
}

impl Lifecycle for UsageMeterPlugin {
    async fn activate(&self, context: ActivateContext) -> Result<(), RuntimeFailure> {
        let dependencies = context.dependencies().clone();
        let cancellation = context.cancellation();
        let database_url = resolve_secret(
            &self.secrets,
            &dependencies,
            cancellation,
            &self.config.database_url_secret,
        )
        .await?;
        let postgres = OwnedPostgres::prepare(
            &database_url,
            schema_plan(self.config.schema.clone()).map_err(|error| {
                RuntimeFailure::InvalidResolvedPlan {
                    detail: error.to_string(),
                }
            })?,
        )
        .await
        .map_err(|error| RuntimeFailure::PluginFailure {
            detail: error.to_string(),
        })?;
        self.state.replace(Some(PreparedUsageMeter { postgres }));
        Ok(())
    }

    async fn deactivate(&self, _context: DeactivateContext) -> Result<(), RuntimeFailure> {
        let prepared = self.state.borrow_mut().take();
        if let Some(prepared) = prepared {
            prepared.postgres.pool().close().await;
        }
        Ok(())
    }
}

async fn ensure_aggregate(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_kind: &str,
    scope_id: &str,
    subject: &str,
    meter: &str,
) -> Result<(), RuntimeFailure> {
    sqlx::query("INSERT INTO usage_aggregates(scope_kind,scope_id,subject,meter_key) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
        .bind(scope_kind).bind(scope_id).bind(subject).bind(meter)
        .execute(&mut **transaction).await
        .map_err(|source| runtime(UsageMeterError::Database { operation: "ensure usage aggregate", source }))?;
    Ok(())
}

async fn lock_revision(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_kind: &str,
    scope_id: &str,
    subject: &str,
    meter: &str,
) -> Result<i64, RuntimeFailure> {
    sqlx::query_scalar("SELECT aggregate_revision FROM usage_aggregates WHERE scope_kind=$1 AND scope_id=$2 AND subject=$3 AND meter_key=$4 FOR UPDATE")
        .bind(scope_kind).bind(scope_id).bind(subject).bind(meter)
        .fetch_one(&mut **transaction).await
        .map_err(|source| runtime(UsageMeterError::Database { operation: "lock usage aggregate", source }))
}

async fn advance_revision(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_kind: &str,
    scope_id: &str,
    subject: &str,
    meter: &str,
) -> Result<i64, RuntimeFailure> {
    sqlx::query_scalar("UPDATE usage_aggregates SET aggregate_revision=aggregate_revision+1 WHERE scope_kind=$1 AND scope_id=$2 AND subject=$3 AND meter_key=$4 RETURNING aggregate_revision")
        .bind(scope_kind).bind(scope_id).bind(subject).bind(meter)
        .fetch_one(&mut **transaction).await
        .map_err(|source| runtime(UsageMeterError::Database { operation: "advance usage revision", source }))
}

fn decode<T>(
    row: &sqlx::postgres::PgRow,
    column: &'static str,
    operation: &'static str,
) -> Result<T, RuntimeFailure>
where
    for<'row> T: sqlx::Decode<'row, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(column)
        .map_err(|source| runtime(UsageMeterError::Database { operation, source }))
}

#[derive(Debug, Error)]
enum UsageMeterError {
    #[error("PostgreSQL operation `{operation}` failed")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

fn runtime(error: impl fmt::Display) -> RuntimeFailure {
    RuntimeFailure::PluginFailure {
        detail: error.to_string(),
    }
}

fn caller_allowed(context: &InvocationContext, allowed: &[String]) -> bool {
    context
        .caller_instance()
        .is_some_and(|caller| allowed.iter().any(|candidate| candidate == caller))
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn valid_dimension(value: &str) -> bool {
    valid_name(value) && !value.starts_with('.') && !value.ends_with('.')
}

fn pg_timestamp_representable(value: &OffsetDateTime) -> bool {
    value.nanosecond().is_multiple_of(1_000)
}

fn unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

fn valid_secret_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 256
        && !reference.starts_with('/')
        && !reference.ends_with('/')
        && !reference.contains("//")
        && reference
            .split('/')
            .all(|segment| segment != "." && segment != "..")
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
}

async fn resolve_secret(
    secrets: &SecretsClient,
    dependencies: &lenso_kernel::PluginDependencies,
    cancellation: lenso_kernel::CancellationToken,
    reference: &str,
) -> Result<Zeroizing<String>, RuntimeFailure> {
    let context = dependencies.invocation_context_after(DEPENDENCY_TIMEOUT, cancellation)?;
    secrets
        .resolve_with_context(
            context,
            ResolveRequest {
                reference: reference.to_owned(),
            },
        )
        .await
        .map(|value| Zeroizing::new(value.value))
        .map_err(|error| match error {
            SecretsInvocationError::Domain(_) => RuntimeFailure::PluginFailure {
                detail: format!("secret `{reference}` was rejected"),
            },
            SecretsInvocationError::Runtime(error) => error,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lenso_kernel::CancellationToken;
    use sqlx::{AssertSqlSafe, Executor};

    fn plugin() -> UsageMeterPlugin {
        UsageMeterPlugin {
            config: UsageMeterConfig::new(
                "usage_meter",
                "usage-meter/database",
                vec!["billing-service".to_owned()],
                vec!["business-service".to_owned()],
                vec!["billing-admin".to_owned()],
            )
            .unwrap(),
            secrets: Port::default(),
            state: Rc::new(RefCell::new(None)),
        }
    }

    #[test]
    fn configuration_requires_explicit_mutation_callers() {
        assert_eq!(
            UsageMeterConfig::new(
                "usage",
                "usage/database",
                vec!["billing-service".to_owned()],
                Vec::new(),
                vec!["admin".to_owned()]
            )
            .unwrap_err(),
            UsageMeterConfigError::InvalidProducerCallers
        );
    }

    #[test]
    fn configuration_requires_explicit_readers() {
        assert_eq!(
            UsageMeterConfig::new(
                "usage",
                "usage/database",
                Vec::new(),
                vec!["producer".to_owned()],
                vec!["admin".to_owned()],
            )
            .unwrap_err(),
            UsageMeterConfigError::InvalidReaderCallers
        );
    }

    #[tokio::test]
    async fn untrusted_producer_is_rejected_before_storage_access() {
        let context = InvocationContext::new(1, None, CancellationToken::new())
            .with_caller_instance("untrusted");
        let result = plugin()
            .record_usage(
                context,
                RecordUsageRequest {
                    event_id: "evt_1".to_owned(),
                    scope_kind: "organization".to_owned(),
                    scope_id: "org_acme".to_owned(),
                    subject: "org_acme".to_owned(),
                    meter: "api.requests".to_owned(),
                    quantity: "1".to_owned(),
                    occurred_at: "2026-08-30T00:00:00Z".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result, Err(RecordUsageError::Forbidden));
    }

    #[tokio::test]
    async fn malformed_window_is_a_domain_error_before_storage_access() {
        let result = plugin()
            .read_usage_window(
                InvocationContext::new(1, None, CancellationToken::new())
                    .with_caller_instance("billing-service"),
                ReadUsageWindowRequest {
                    scope_kind: "organization".to_owned(),
                    scope_id: "org_acme".to_owned(),
                    subject: "org_acme".to_owned(),
                    meter: "api.requests".to_owned(),
                    window_start: "2026-09-01T00:00:00Z".to_owned(),
                    window_end: "2026-08-01T00:00:00Z".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result, Err(ReadUsageWindowError::InvalidWindow));
    }

    #[tokio::test]
    async fn untrusted_reader_is_rejected_before_storage_access() {
        let result = plugin()
            .read_usage_window(
                InvocationContext::new(1, None, CancellationToken::new())
                    .with_caller_instance("untrusted"),
                ReadUsageWindowRequest {
                    scope_kind: "organization".to_owned(),
                    scope_id: "org_acme".to_owned(),
                    subject: "org_acme".to_owned(),
                    meter: "api.requests".to_owned(),
                    window_start: "2026-08-01T00:00:00Z".to_owned(),
                    window_end: "2026-09-01T00:00:00Z".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result, Err(ReadUsageWindowError::Forbidden));
    }

    #[tokio::test]
    async fn sub_microsecond_event_is_rejected_before_storage_access() {
        let result = plugin()
            .record_usage(
                InvocationContext::new(1, None, CancellationToken::new())
                    .with_caller_instance("business-service"),
                RecordUsageRequest {
                    event_id: "evt_1".to_owned(),
                    scope_kind: "organization".to_owned(),
                    scope_id: "org_acme".to_owned(),
                    subject: "org_acme".to_owned(),
                    meter: "api.requests".to_owned(),
                    quantity: "1".to_owned(),
                    occurred_at: "2026-08-30T00:00:00.000000001Z".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(result, Err(RecordUsageError::InvalidEvent));
    }

    #[tokio::test]
    #[ignore = "requires LENSO_POSTGRES_TEST_URL"]
    async fn replay_correction_and_window_are_consistent() {
        let database_url =
            std::env::var("LENSO_POSTGRES_TEST_URL").expect("LENSO_POSTGRES_TEST_URL is required");
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("usage_meter_test_{}_{suffix}", std::process::id());
        UsageMeterOperator::setup(&database_url, &schema)
            .await
            .unwrap();
        let postgres = OwnedPostgres::prepare(&database_url, schema_plan(schema.clone()).unwrap())
            .await
            .unwrap();
        let plugin = plugin();
        plugin.state.replace(Some(PreparedUsageMeter { postgres }));
        let producer = InvocationContext::new(1, None, CancellationToken::new())
            .with_caller_instance("business-service");
        let event = RecordUsageRequest {
            event_id: "evt_1".to_owned(),
            scope_kind: "organization".to_owned(),
            scope_id: "org_acme".to_owned(),
            subject: "org_acme".to_owned(),
            meter: "api.requests".to_owned(),
            quantity: "5".to_owned(),
            occurred_at: "2026-08-30T00:00:00Z".to_owned(),
        };
        let first = plugin
            .record_usage(producer.clone(), event.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(first.accepted);
        assert_eq!(first.aggregate_revision, 1);
        let replay = plugin.record_usage(producer, event).await.unwrap().unwrap();
        assert!(!replay.accepted);
        assert_eq!(replay.aggregate_revision, 1);
        let admin = InvocationContext::new(2, None, CancellationToken::new())
            .with_caller_instance("billing-admin");
        let corrected = plugin
            .correct_usage(
                admin,
                CorrectUsageRequest {
                    correction_id: "cor_1".to_owned(),
                    original_event_id: "evt_1".to_owned(),
                    quantity_delta: "-2".to_owned(),
                    reason: "duplicate downstream work".to_owned(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(corrected.accepted);
        assert_eq!(corrected.aggregate_revision, 2);
        let window = plugin
            .read_usage_window(
                InvocationContext::new(3, None, CancellationToken::new())
                    .with_caller_instance("billing-service"),
                ReadUsageWindowRequest {
                    scope_kind: "organization".to_owned(),
                    scope_id: "org_acme".to_owned(),
                    subject: "org_acme".to_owned(),
                    meter: "api.requests".to_owned(),
                    window_start: "2026-08-01T00:00:00Z".to_owned(),
                    window_end: "2026-09-01T00:00:00Z".to_owned(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(window.quantity, "3");
        assert_eq!(window.event_count, 1);
        assert_eq!(window.aggregate_revision, 2);

        let cleanup_pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        cleanup_pool
            .execute(AssertSqlSafe(format!("DROP SCHEMA \"{schema}\" CASCADE")))
            .await
            .unwrap();
        cleanup_pool.close().await;
    }
}
