use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ullage_auth::{AuthChallenge, AuthCompleteRequest, AuthStartRequest, AuthState, LogoutRequest};

use crate::{
    ProviderError, ProviderErrorKind, ProviderResult, QueryOutcome, SubscriptionUsage,
    is_unsafe_identity_character,
};

/// Account key used by `ProviderRegistry::get` and by providers that store a
/// single credential per provider.
pub const DEFAULT_ACCOUNT_ID: &str = "active";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Builds an id without validation; `ProviderRegistry` rejects unusable
    /// ids at registration. Callers that need validation up front can use
    /// [`ProviderId::is_usable`].
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the id is non-empty and free of control or bidirectional
    /// characters that could obscure it in logs and labels.
    pub fn is_usable(&self) -> bool {
        !self.0.is_empty() && !self.0.chars().any(is_unsafe_identity_character)
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Authentication,
    AuthenticationStatus,
    Logout,
    UsageQuery,
    SubscriptionExpiry,
    WorkspaceSelection,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderWorkspace {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageQuery {
    pub account_label: Option<String>,
}

/// Typed provider contract. Each implementation keeps its vendor DTO in its own crate.
#[async_trait]
pub trait Provider: Send + Sync {
    type VendorUsage: Send;

    fn descriptor(&self) -> ProviderDescriptor;
    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge>;
    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState>;
    async fn auth_status(&self) -> ProviderResult<AuthState>;
    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()>;
    async fn list_workspaces(&self) -> ProviderResult<Vec<ProviderWorkspace>> {
        Err(ProviderError::UnsupportedCapability {
            capability: "workspace selection".into(),
        })
    }
    async fn select_workspace(&self, _workspace_id: &str) -> ProviderResult<ProviderWorkspace> {
        Err(ProviderError::UnsupportedCapability {
            capability: "workspace selection".into(),
        })
    }
    async fn query(&self, request: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>>;
    fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage>;
}

/// Object-safe form used only at the registry boundary.
#[async_trait]
pub trait RegisteredProvider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge>;
    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState>;
    async fn auth_status(&self) -> ProviderResult<AuthState>;
    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()>;
    async fn list_workspaces(&self) -> ProviderResult<Vec<ProviderWorkspace>>;
    async fn select_workspace(&self, workspace_id: &str) -> ProviderResult<ProviderWorkspace>;
    async fn query_usage(
        &self,
        request: UsageQuery,
    ) -> ProviderResult<QueryOutcome<SubscriptionUsage>>;
}

#[async_trait]
impl<T> RegisteredProvider for T
where
    T: Provider + 'static,
{
    fn descriptor(&self) -> ProviderDescriptor {
        Provider::descriptor(self)
    }

    async fn start_auth(&self, request: AuthStartRequest) -> ProviderResult<AuthChallenge> {
        Provider::start_auth(self, request).await
    }

    async fn complete_auth(&self, request: AuthCompleteRequest) -> ProviderResult<AuthState> {
        Provider::complete_auth(self, request).await
    }

    async fn auth_status(&self) -> ProviderResult<AuthState> {
        Provider::auth_status(self).await
    }

    async fn logout(&self, request: LogoutRequest) -> ProviderResult<()> {
        Provider::logout(self, request).await
    }

    async fn list_workspaces(&self) -> ProviderResult<Vec<ProviderWorkspace>> {
        Provider::list_workspaces(self).await
    }

    async fn select_workspace(&self, workspace_id: &str) -> ProviderResult<ProviderWorkspace> {
        Provider::select_workspace(self, workspace_id).await
    }

    async fn query_usage(
        &self,
        request: UsageQuery,
    ) -> ProviderResult<QueryOutcome<SubscriptionUsage>> {
        Provider::query(self, request)
            .await?
            .try_map(|raw| Provider::normalize(self, raw))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "provider", rename_all = "snake_case")]
pub enum RegistryError {
    #[error("provider is already registered: {0}")]
    Duplicate(ProviderId),
    #[error("provider is not registered: {0}")]
    NotFound(ProviderId),
    /// The id cannot safely name a registry entry; its text is untrusted, so
    /// it is carried for debugging but the message does not echo it.
    #[error("provider identifier is not usable")]
    InvalidId(ProviderId),
    /// The factory produced an instance whose descriptor names a different
    /// provider; refusing it keeps lookups from returning the wrong adapter.
    #[error("provider factory returned an instance for {actual} instead of {registered}")]
    DescriptorMismatch {
        registered: ProviderId,
        actual: ProviderId,
    },
    /// The factory ran and failed; `kind` preserves the provider error
    /// category while the vendor text stays in the diagnostics channel.
    #[error("provider account instance could not be initialized: {provider} ({kind:?})")]
    InstanceFailed {
        provider: ProviderId,
        kind: ProviderErrorKind,
    },
    /// Unclassifiable initialization failure (for example a poisoned cache).
    #[error("provider account instance could not be initialized: {0}")]
    InstanceUnavailable(ProviderId),
}

type ProviderFactory =
    dyn Fn(&str) -> ProviderResult<Arc<dyn RegisteredProvider>> + Send + Sync + 'static;

enum ProviderRegistration {
    Singleton(Arc<dyn RegisteredProvider>),
    Factory {
        descriptor: ProviderDescriptor,
        factory: Arc<ProviderFactory>,
        instances: Mutex<BTreeMap<String, Arc<dyn RegisteredProvider>>>,
    },
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<ProviderId, ProviderRegistration>,
}

impl ProviderRegistry {
    pub fn register<P>(&mut self, provider: P) -> Result<(), RegistryError>
    where
        P: Provider + 'static,
    {
        self.register_arc(Arc::new(provider))
    }

    pub fn register_arc(
        &mut self,
        provider: Arc<dyn RegisteredProvider>,
    ) -> Result<(), RegistryError> {
        let id = provider.descriptor().id;
        if !id.is_usable() {
            return Err(RegistryError::InvalidId(id));
        }
        if self.providers.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.providers
            .insert(id, ProviderRegistration::Singleton(provider));
        Ok(())
    }

    pub fn register_factory<F>(
        &mut self,
        descriptor: ProviderDescriptor,
        factory: F,
    ) -> Result<(), RegistryError>
    where
        F: Fn(&str) -> ProviderResult<Arc<dyn RegisteredProvider>> + Send + Sync + 'static,
    {
        let id = descriptor.id.clone();
        if !id.is_usable() {
            return Err(RegistryError::InvalidId(id));
        }
        if self.providers.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.providers.insert(
            id,
            ProviderRegistration::Factory {
                descriptor,
                factory: Arc::new(factory),
                instances: Mutex::new(BTreeMap::new()),
            },
        );
        Ok(())
    }

    pub fn get(&self, id: &ProviderId) -> Result<Arc<dyn RegisteredProvider>, RegistryError> {
        self.get_for_account(id, DEFAULT_ACCOUNT_ID)
    }

    /// Returns the cached instance for `(id, account_id)`, constructing one on
    /// a miss. The factory runs without the cache lock held — factories may do
    /// slow work such as credential-store access. If two callers race, exactly
    /// one instance is published and the other's is dropped.
    pub fn get_for_account(
        &self,
        id: &ProviderId,
        account_id: &str,
    ) -> Result<Arc<dyn RegisteredProvider>, RegistryError> {
        match self
            .providers
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?
        {
            ProviderRegistration::Singleton(provider) => Ok(provider.clone()),
            ProviderRegistration::Factory {
                factory, instances, ..
            } => {
                let fast_path = instances
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(account_id)
                    .cloned();
                if let Some(provider) = fast_path {
                    return Ok(provider);
                }
                let provider =
                    factory(account_id).map_err(|error| RegistryError::InstanceFailed {
                        provider: id.clone(),
                        kind: error.kind(),
                    })?;
                let actual = provider.descriptor().id;
                if actual != *id {
                    return Err(RegistryError::DescriptorMismatch {
                        registered: id.clone(),
                        actual,
                    });
                }
                match instances
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(account_id.to_owned())
                {
                    Entry::Occupied(entry) => Ok(entry.get().clone()),
                    Entry::Vacant(entry) => Ok(entry.insert(provider).clone()),
                }
            }
        }
    }

    pub fn contains(&self, id: &ProviderId) -> bool {
        self.providers.contains_key(id)
    }

    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.providers
            .values()
            .map(|provider| match provider {
                ProviderRegistration::Singleton(provider) => provider.descriptor(),
                ProviderRegistration::Factory { descriptor, .. } => descriptor.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use chrono::Utc;

    use super::*;
    use crate::{PartialFailure, ProviderError};

    struct StubProvider;

    #[async_trait]
    impl Provider for StubProvider {
        type VendorUsage = SubscriptionUsage;

        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                id: ProviderId::new("stub"),
                display_name: "Stub".into(),
                capabilities: vec![Capability::UsageQuery],
            }
        }

        async fn start_auth(&self, _: AuthStartRequest) -> ProviderResult<AuthChallenge> {
            Err(ProviderError::UnsupportedCapability {
                capability: "authentication".into(),
            })
        }

        async fn complete_auth(&self, _: AuthCompleteRequest) -> ProviderResult<AuthState> {
            Err(ProviderError::UnsupportedCapability {
                capability: "authentication".into(),
            })
        }

        async fn auth_status(&self) -> ProviderResult<AuthState> {
            Ok(AuthState::NotAuthenticated)
        }

        async fn logout(&self, _: LogoutRequest) -> ProviderResult<()> {
            Ok(())
        }

        async fn query(&self, _: UsageQuery) -> ProviderResult<QueryOutcome<Self::VendorUsage>> {
            Ok(QueryOutcome::Partial {
                data: SubscriptionUsage {
                    provider: ProviderId::new("stub"),
                    account_label: None,
                    plan: None,
                    subscription_expires_at: None,
                    observed_at: Utc::now(),
                    windows: Vec::new(),
                },
                failures: vec![PartialFailure {
                    scope: "weekly".into(),
                    message: "temporarily unavailable".into(),
                }],
            })
        }

        fn normalize(&self, vendor_usage: Self::VendorUsage) -> ProviderResult<SubscriptionUsage> {
            Ok(vendor_usage)
        }
    }

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("stub future unexpectedly yielded"),
        }
    }

    #[test]
    fn rejects_duplicate_provider_ids() {
        let mut registry = ProviderRegistry::default();
        registry.register(StubProvider).unwrap();

        assert_eq!(
            registry.register(StubProvider).unwrap_err(),
            RegistryError::Duplicate(ProviderId::new("stub"))
        );
        assert_eq!(registry.descriptors().len(), 1);
    }

    #[test]
    fn distinguishes_missing_providers() {
        let registry = ProviderRegistry::default();
        let missing = ProviderId::new("missing");

        assert!(matches!(
            registry.get(&missing),
            Err(RegistryError::NotFound(id)) if id == missing
        ));
    }

    #[test]
    fn object_safe_adapter_preserves_partial_failures() {
        let outcome = run_ready(RegisteredProvider::query_usage(
            &StubProvider,
            UsageQuery::default(),
        ))
        .unwrap();

        assert!(matches!(
            outcome,
            QueryOutcome::Partial { data, failures }
                if data.provider == ProviderId::new("stub")
                    && failures[0].scope == "weekly"
        ));
    }

    #[test]
    fn factory_caches_distinct_provider_instances_per_account() {
        let mut registry = ProviderRegistry::default();
        registry
            .register_factory(Provider::descriptor(&StubProvider), |_| {
                Ok(Arc::new(StubProvider))
            })
            .unwrap();
        let id = ProviderId::new("stub");
        let first = registry.get_for_account(&id, "account-a").unwrap();
        let repeated = registry.get_for_account(&id, "account-a").unwrap();
        let second = registry.get_for_account(&id, "account-b").unwrap();

        assert!(Arc::ptr_eq(&first, &repeated));
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
