//! Stable domain contracts shared by the daemon and provider implementations.

mod error;
mod provider;
mod usage;

pub use error::{
    LEGACY_PARTIAL_FAILURE_MESSAGE, PartialFailure, ProviderError, ProviderResult, QueryOutcome,
};
pub use provider::{
    Capability, Provider, ProviderDescriptor, ProviderId, ProviderRegistry, ProviderWorkspace,
    RegisteredProvider, RegistryError, UsageQuery,
};
pub use usage::{
    MeasurementUnit, SubscriptionUsage, UsageMeasurement, UsageWindow, UsageWindowKind,
};
