//! Stable domain contracts shared by the daemon and provider implementations.

mod error;
mod provider;
pub mod summary;
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

/// Whether a user-supplied identity string may not be used as configuration or
/// display text.
///
/// Control characters and bidirectional text controls can hide or reorder what
/// a person sees, so account identities and metric filter names reject them.
pub fn is_unsafe_identity_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}
