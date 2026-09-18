//! Production provider registry: compiled-in ids, factories, and the
//! credential store each account-scoped provider instance is built on.

use std::sync::Arc;

use ullage_auth::CredentialStore;
use ullage_core::{
    Capability, ProviderDescriptor, ProviderError, ProviderId, ProviderRegistry, RegisteredProvider,
};
use ullage_protocol::CredentialBackendId;
use ullage_provider_chatgpt::{
    ChatGptConfig, ChatGptHttpConfig, ChatGptProvider, ReqwestChatGptApi,
};
use ullage_provider_grok::{GrokProvider, HttpGrokConfig, HttpGrokTransport};

use crate::config::AppConfig;
use crate::credential_backend::assemble_credential_store;
use crate::credentials::{ChatGptVault, ClaudeVault};

/// The public OAuth client the Codex CLI registers with `auth.openai.com`.
/// ChatGPT sign-in only accepts clients that OpenAI knows about, so the value
/// has to be a registered identifier rather than a name of our own choosing.
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The callback registered for that client. OpenAI compares redirect URIs
/// verbatim, so `localhost` cannot be spelled `127.0.0.1` here.
const CHATGPT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

const PROVIDER_CLAUDE: &str = "claude";
const PROVIDER_CHATGPT: &str = "chatgpt";
const PROVIDER_GROK: &str = "grok";
const PROVIDER_CURSOR: &str = "cursor";
const PROVIDER_OPENCODE: &str = "opencode";
const PROVIDER_DEVIN: &str = "devin";

/// The compiled-in provider ids, shared by the registry construction and
/// `config` validation so a new provider cannot silently skip either.
pub(crate) const PROVIDER_IDS: [&str; 6] = [
    PROVIDER_CLAUDE,
    PROVIDER_CHATGPT,
    PROVIDER_GROK,
    PROVIDER_CURSOR,
    PROVIDER_OPENCODE,
    PROVIDER_DEVIN,
];

pub fn production_registry(config: &AppConfig) -> Result<ProviderRegistry, String> {
    Ok(production_components(config)?.0)
}

pub(crate) fn production_components(
    config: &AppConfig,
) -> Result<(ProviderRegistry, CredentialBackendId), String> {
    let (store, backend) = assemble_credential_store(&config.credentials)?;
    Ok((registry_with_credentials(store)?, backend))
}

pub fn registry_with_credentials(
    credentials: Arc<CredentialStore>,
) -> Result<ProviderRegistry, String> {
    let mut registry = ProviderRegistry::default();
    let claude_api = Arc::new(
        ullage_provider_claude::HttpClaudeApi::new()
            .map_err(|_| "Claude provider initialization failed")?,
    );
    let claude_credentials = credentials.clone();
    registry
        .register_factory(
            descriptor(PROVIDER_CLAUDE, "Claude", true),
            move |account_id| {
                let store = Arc::new(
                    ClaudeVault::new(claude_credentials.clone(), account_id)
                        .map_err(|_| credential_init_error())?,
                );
                Ok(Arc::new(ullage_provider_claude::ClaudeProvider::with_api(
                    claude_api.clone(),
                    store,
                )) as Arc<dyn RegisteredProvider>)
            },
        )
        .map_err(|_| "Claude provider registration failed")?;

    let chatgpt_config = ChatGptConfig::openai(CHATGPT_CLIENT_ID, CHATGPT_REDIRECT_URI);
    let chatgpt_api = Arc::new(
        ReqwestChatGptApi::new(ChatGptHttpConfig::openai(CHATGPT_CLIENT_ID))
            .map_err(|_| "ChatGPT provider initialization failed")?,
    );
    let chatgpt_credentials = credentials.clone();
    let mut chatgpt_descriptor = descriptor(PROVIDER_CHATGPT, "ChatGPT", false);
    chatgpt_descriptor
        .capabilities
        .push(Capability::WorkspaceSelection);
    registry
        .register_factory(chatgpt_descriptor, move |account_id| {
            let store = Arc::new(
                ChatGptVault::new(chatgpt_credentials.clone(), account_id)
                    .map_err(|_| credential_init_error())?,
            );
            Ok(Arc::new(ChatGptProvider::new(
                chatgpt_config.clone(),
                chatgpt_api.clone(),
                store,
            )?) as Arc<dyn RegisteredProvider>)
        })
        .map_err(|_| "ChatGPT provider registration failed")?;

    let grok_transport = Arc::new(
        HttpGrokTransport::new(HttpGrokConfig {
            client_id: "b1a00492-073a-47ea-816f-4c329264a828".into(),
            scope: "openid profile email offline_access grok-cli:access api:access \
                     conversations:read conversations:write workspaces:read workspaces:write"
                .into(),
            // Kept for an explicit browser OAuth request. The macOS app no
            // longer drives that path: auth.x.ai does not bounce back, so Grok
            // defaults to device code.
            redirect_uri: "http://127.0.0.1:1456/callback".into(),
            device_authorization_url: "https://auth.x.ai/oauth2/device/code".into(),
            authorization_url: "https://auth.x.ai/oauth2/authorize".into(),
            token_url: "https://auth.x.ai/oauth2/token".into(),
            revoke_url: "https://auth.x.ai/oauth2/revoke".into(),
            billing_url: "https://cli-chat-proxy.grok.com/v1/billing?format=credits".into(),
            settings_url: "https://cli-chat-proxy.grok.com/v1/settings".into(),
        })
        .map_err(|_| "Grok provider initialization failed")?,
    );
    let grok_credentials = credentials.clone();
    registry
        .register_factory(
            descriptor(PROVIDER_GROK, "Grok", false),
            move |account_id| {
                Ok(Arc::new(GrokProvider::with_transport_and_store_for_account(
                    grok_transport.clone(),
                    grok_credentials.clone(),
                    account_id,
                )?) as Arc<dyn RegisteredProvider>)
            },
        )
        .map_err(|_| "Grok provider registration failed")?;

    let opencode_credentials = credentials.clone();
    let devin_credentials = credentials.clone();
    let cursor_api: Arc<dyn ullage_provider_cursor::CursorApi> = Arc::new(
        ullage_provider_cursor::HttpCursorApi::new()
            .map_err(|_| "Cursor provider initialization failed")?,
    );
    registry
        .register_factory(
            descriptor(PROVIDER_CURSOR, "Cursor", false),
            move |account_id| {
                Ok(Arc::new(
                    ullage_provider_cursor::CursorProvider::with_api_and_store_for_account(
                        cursor_api.clone(),
                        credentials.clone(),
                        account_id,
                    )?,
                ) as Arc<dyn RegisteredProvider>)
            },
        )
        .map_err(|_| "Cursor provider registration failed")?;

    let opencode_api: Arc<dyn ullage_provider_opencode::OpencodeApi> = Arc::new(
        ullage_provider_opencode::HttpOpencodeApi::new()
            .map_err(|_| "OpenCode provider initialization failed")?,
    );
    registry
        .register_factory(
            descriptor(PROVIDER_OPENCODE, "OpenCode", false),
            move |account_id| {
                Ok(Arc::new(
                    ullage_provider_opencode::OpencodeProvider::with_api_and_store_for_account(
                        opencode_api.clone(),
                        opencode_credentials.clone(),
                        account_id,
                    )?,
                ) as Arc<dyn RegisteredProvider>)
            },
        )
        .map_err(|_| "OpenCode provider registration failed")?;

    let devin_api: Arc<dyn ullage_provider_devin::DevinApi> = Arc::new(
        ullage_provider_devin::HttpDevinApi::new()
            .map_err(|_| "Devin provider initialization failed")?,
    );
    registry
        .register_factory(
            descriptor(PROVIDER_DEVIN, "Devin", false),
            move |account_id| {
                Ok(Arc::new(
                    ullage_provider_devin::DevinProvider::with_api_and_store_for_account(
                        devin_api.clone(),
                        devin_credentials.clone(),
                        account_id,
                    )?,
                ) as Arc<dyn RegisteredProvider>)
            },
        )
        .map_err(|_| "Devin provider registration failed")?;
    Ok(registry)
}

fn descriptor(id: &str, display_name: &str, subscription_expiry: bool) -> ProviderDescriptor {
    let mut capabilities = vec![
        Capability::Authentication,
        Capability::AuthenticationStatus,
        Capability::Logout,
        Capability::UsageQuery,
    ];
    if subscription_expiry {
        capabilities.push(Capability::SubscriptionExpiry);
    }
    ProviderDescriptor {
        id: ProviderId::new(id),
        display_name: display_name.into(),
        capabilities,
    }
}

fn credential_init_error() -> ProviderError {
    ProviderError::ProtocolIncompatible {
        message: "credential account identity is invalid".into(),
    }
}
