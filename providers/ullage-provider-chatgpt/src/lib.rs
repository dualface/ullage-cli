//! ChatGPT/Codex OAuth, workspace, and usage provider.

mod dto;
mod error;
mod http;
mod provider;

pub use dto::{
    ChatGptAdditionalRateLimit, ChatGptCredits, ChatGptRateLimit, ChatGptRateLimitWindow,
    ChatGptResetCredits, ChatGptUsage, ChatGptUsageResponse, ChatGptWorkspace,
};
pub use error::{ChatGptApiError, ChatGptApiErrorKind};
pub use http::{ChatGptHttpConfig, ReqwestChatGptApi};
pub use provider::{
    ChatGptApi, ChatGptConfig, ChatGptProvider, ChatGptSession, ChatGptSessionStore, OAuthTokenSet,
};

pub fn normalize(
    usage: ChatGptUsage,
) -> ullage_core::ProviderResult<ullage_core::SubscriptionUsage> {
    usage.normalize()
}
