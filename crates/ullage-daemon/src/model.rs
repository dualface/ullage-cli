use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ullage_core::{
    ProviderError, ProviderId, QueryOutcome, RegistryError, SubscriptionUsage, UsageQuery,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub maximum: Duration,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(30),
            maximum: Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: AccountId,
    pub provider: ProviderId,
    pub query: UsageQuery,
    pub enabled: bool,
    pub interval: Duration,
    pub timeout: Duration,
    pub jitter: Duration,
    pub backoff: BackoffConfig,
}

#[derive(Clone, Debug)]
pub struct ProviderLimit {
    pub provider: ProviderId,
    pub maximum_concurrency: usize,
}

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub maximum_concurrency: usize,
    pub default_provider_concurrency: usize,
    pub provider_limits: Vec<ProviderLimit>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            maximum_concurrency: 8,
            default_provider_concurrency: 2,
            provider_limits: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeTrigger {
    Startup,
    Periodic,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitizedError {
    AuthenticationInvalid,
    RateLimited { retry_after_seconds: Option<u64> },
    Network,
    ProtocolIncompatible,
    UnsupportedCapability,
    Timeout,
    Cancelled,
    ProviderNotFound,
    Storage,
}

impl SanitizedError {
    pub(crate) fn from_probe(error: &ProbeError) -> Self {
        match error {
            ProbeError::Provider(ProviderError::AuthenticationInvalid { .. }) => {
                Self::AuthenticationInvalid
            }
            ProbeError::Provider(ProviderError::RateLimited {
                retry_after_seconds,
                ..
            }) => Self::RateLimited {
                retry_after_seconds: *retry_after_seconds,
            },
            ProbeError::Provider(ProviderError::Network { .. }) => Self::Network,
            ProbeError::Provider(ProviderError::ProtocolIncompatible { .. }) => {
                Self::ProtocolIncompatible
            }
            ProbeError::Provider(ProviderError::UnsupportedCapability { .. }) => {
                Self::UnsupportedCapability
            }
            ProbeError::Timeout => Self::Timeout,
            ProbeError::Cancelled => Self::Cancelled,
            ProbeError::Registry(_) => Self::ProviderNotFound,
            ProbeError::AccountNotFound(_) => Self::ProviderNotFound,
            ProbeError::Storage(_) => Self::Storage,
        }
    }
}

pub(crate) fn sanitize_provider_error(error: ProviderError) -> ProviderError {
    error.sanitized()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub account_id: AccountId,
    pub usage: QueryOutcome<SubscriptionUsage>,
    pub last_success_at: DateTime<Utc>,
    pub stale: bool,
    pub last_error: Option<SanitizedError>,
    pub last_error_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub error: SanitizedError,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub accounts: BTreeMap<AccountId, AccountConfig>,
    #[serde(default)]
    pub removed_accounts: std::collections::BTreeSet<AccountId>,
    #[serde(default = "default_next_account_sequence")]
    pub next_account_sequence: u64,
    #[serde(default)]
    pub snapshots: BTreeMap<AccountId, SnapshotRecord>,
    #[serde(default)]
    pub failures: BTreeMap<AccountId, FailureRecord>,
}

fn default_next_account_sequence() -> u64 {
    1
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            accounts: BTreeMap::new(),
            removed_accounts: std::collections::BTreeSet::new(),
            next_account_sequence: default_next_account_sequence(),
            snapshots: BTreeMap::new(),
            failures: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountStatus {
    pub id: AccountId,
    pub provider: ProviderId,
    pub enabled: bool,
    pub in_flight: bool,
    pub consecutive_failures: u32,
    pub next_probe_at: Option<DateTime<Utc>>,
    pub has_snapshot: bool,
    pub stale: bool,
    pub last_error: Option<SanitizedError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonStatus {
    pub shutting_down: bool,
    pub accounts: Vec<AccountStatus>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProbeError {
    #[error("account is not configured: {0}")]
    AccountNotFound(AccountId),
    #[error("provider registry error: {0}")]
    Registry(RegistryError),
    #[error("provider request failed: {0}")]
    Provider(ProviderError),
    #[error("provider request timed out")]
    Timeout,
    #[error("daemon is shutting down")]
    Cancelled,
    #[error("snapshot storage failed: {0}")]
    Storage(String),
}

impl From<RegistryError> for ProbeError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<ProviderError> for ProbeError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("global concurrency must be greater than zero")]
    InvalidGlobalConcurrency,
    #[error("default provider concurrency must be greater than zero")]
    InvalidProviderConcurrency,
    #[error("provider concurrency must be greater than zero: {0}")]
    InvalidProviderLimit(ProviderId),
    #[error("account interval must be greater than zero: {0}")]
    InvalidInterval(AccountId),
    #[error("account timeout must be greater than zero: {0}")]
    InvalidTimeout(AccountId),
    #[error("account backoff is invalid: {0}")]
    InvalidBackoff(AccountId),
    #[error("account is already configured: {0}")]
    DuplicateAccount(AccountId),
    #[error("account control selector is already configured for provider {provider}")]
    DuplicateAccountSelector {
        provider: ProviderId,
        account_label: Option<String>,
    },
    #[error("snapshot storage failed: {0}")]
    Storage(String),
    #[error("daemon is shutting down")]
    Cancelled,
}

pub(crate) type SnapshotMap = BTreeMap<AccountId, SnapshotRecord>;
