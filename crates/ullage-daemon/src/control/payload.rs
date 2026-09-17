//! Control-protocol payload projection for account, status, and snapshot data.

use ullage_protocol::{
    Account, AccountError, AccountId as ProtocolAccountId, AccountStatusPayload, ControlError,
    CredentialBackendId, DaemonStatusPayload, SanitizedErrorPayload, SnapshotPayload,
};

use crate::model::sanitize_provider_error;
use crate::{
    AccountConfig, AccountId, DaemonEngine, DaemonError, DaemonStatus, ProbeError, SanitizedError,
    SnapshotRecord,
};

pub(super) fn account_payload(config: AccountConfig) -> Account {
    Account {
        id: ProtocolAccountId::new(config.id.to_string()),
        provider: config.provider,
        label: config.query.account_label,
        enabled: config.enabled,
        metrics: config.metrics,
    }
}

pub(super) fn account_control_error(error: DaemonError, requested: AccountId) -> ControlError {
    match error {
        DaemonError::DuplicateAccount(account) => {
            AccountError::Duplicate(ProtocolAccountId::new(account.to_string())).into()
        }
        DaemonError::DuplicateAccountSelector { .. } => {
            AccountError::Duplicate(ProtocolAccountId::new(requested.to_string())).into()
        }
        DaemonError::Storage(_) => ControlError::Storage,
        DaemonError::Cancelled => ControlError::Cancelled,
        DaemonError::InvalidAccountMetrics(_) => ControlError::InvalidAccountMetrics,
        DaemonError::InvalidGlobalConcurrency
        | DaemonError::InvalidProviderConcurrency
        | DaemonError::InvalidProviderLimit(_)
        | DaemonError::InvalidInterval(_)
        | DaemonError::InvalidTimeout(_)
        | DaemonError::InvalidBackoff(_) => ControlError::UnsupportedCommand,
    }
}

pub(super) fn probe_control_error(error: ProbeError) -> ControlError {
    match error {
        ProbeError::Provider(error) => ControlError::Provider(sanitize_provider_error(error)),
        ProbeError::Registry(error) => ControlError::Registry(error),
        ProbeError::AccountNotFound(account_id) => ControlError::AccountNotFound {
            account_id: account_id.to_string(),
        },
        ProbeError::Timeout => ControlError::Timeout,
        ProbeError::Cancelled => ControlError::Cancelled,
        ProbeError::Storage(_) => ControlError::Storage,
    }
}

pub(super) fn status_payload(
    status: DaemonStatus,
    credential_backend: CredentialBackendId,
) -> DaemonStatusPayload {
    DaemonStatusPayload {
        shutting_down: status.shutting_down,
        credential_backend,
        accounts: status
            .accounts
            .into_iter()
            .map(|account| AccountStatusPayload {
                account_id: account.id.to_string(),
                provider: account.provider,
                enabled: account.enabled,
                in_flight: account.in_flight,
                consecutive_failures: account.consecutive_failures,
                next_probe_at: account.next_probe_at,
                has_snapshot: account.has_snapshot,
                stale: account.stale,
                last_error: account.last_error.map(sanitized_error_payload),
            })
            .collect(),
    }
}

/// Display metric names persisted for one account; empty when unknown.
pub(super) async fn account_metrics(engine: &DaemonEngine, account_id: &AccountId) -> Vec<String> {
    engine
        .account_config(account_id)
        .await
        .map(|config| config.metrics)
        .unwrap_or_default()
}

pub(super) fn snapshot_payload(snapshot: SnapshotRecord, metrics: &[String]) -> SnapshotPayload {
    SnapshotPayload {
        account_id: snapshot.account_id.to_string(),
        usage: snapshot.usage,
        last_success_at: snapshot.last_success_at,
        stale: snapshot.stale,
        last_error: snapshot.last_error.map(sanitized_error_payload),
        last_error_at: snapshot.last_error_at,
        metrics: metrics.to_vec(),
    }
}

fn sanitized_error_payload(error: SanitizedError) -> SanitizedErrorPayload {
    match error {
        SanitizedError::AuthenticationInvalid => SanitizedErrorPayload::AuthenticationInvalid,
        SanitizedError::RateLimited {
            retry_after_seconds,
        } => SanitizedErrorPayload::RateLimited {
            retry_after_seconds,
        },
        SanitizedError::Network => SanitizedErrorPayload::Network,
        SanitizedError::ProtocolIncompatible => SanitizedErrorPayload::ProtocolIncompatible,
        SanitizedError::UnsupportedCapability => SanitizedErrorPayload::UnsupportedCapability,
        SanitizedError::Timeout => SanitizedErrorPayload::Timeout,
        SanitizedError::Cancelled => SanitizedErrorPayload::Cancelled,
        SanitizedError::ProviderNotFound => SanitizedErrorPayload::ProviderNotFound,
        SanitizedError::AccountNotFound => SanitizedErrorPayload::AccountNotFound,
        SanitizedError::Storage => SanitizedErrorPayload::Storage,
    }
}
