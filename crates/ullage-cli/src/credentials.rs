use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ullage_auth::{Credential, CredentialError, CredentialKey, CredentialStore, SecretValue};
use ullage_provider_chatgpt::{
    ChatGptApiError, ChatGptApiErrorKind, ChatGptSession, ChatGptSessionStore, ChatGptWorkspace,
    OAuthTokenSet,
};
use ullage_provider_claude::{ClaudeCredential, ClaudeCredentialStore};

pub struct ClaudeVault {
    store: Arc<CredentialStore>,
    key: CredentialKey,
}

impl ClaudeVault {
    pub fn new(store: Arc<CredentialStore>, account_id: &str) -> Result<Self, String> {
        Ok(Self {
            store,
            key: CredentialKey::new("claude", account_id).map_err(|error| error.to_string())?,
        })
    }
}

impl ClaudeCredentialStore for ClaudeVault {
    fn load(&self) -> ullage_core::ProviderResult<Option<ClaudeCredential>> {
        let credential: Option<ClaudeCredential> =
            load_json(&self.store, &self.key).map_err(provider_store_error)?;
        credential
            .map(|credential| {
                credential.validate()?;
                Ok(credential)
            })
            .transpose()
    }

    fn save(&self, credential: &ClaudeCredential) -> ullage_core::ProviderResult<()> {
        save_json(&self.store, &self.key, credential).map_err(provider_store_error)
    }

    fn clear(&self) -> ullage_core::ProviderResult<()> {
        delete(&self.store, &self.key).map_err(provider_store_error)
    }
}

pub struct ChatGptVault {
    store: Arc<CredentialStore>,
    key: CredentialKey,
}

impl ChatGptVault {
    pub fn new(store: Arc<CredentialStore>, account_id: &str) -> Result<Self, String> {
        Ok(Self {
            store,
            key: CredentialKey::new("chatgpt", account_id).map_err(|error| error.to_string())?,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedChatGptSession {
    access_token: String,
    refresh_token: Option<String>,
    identity_token: Option<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    workspaces: Vec<ChatGptWorkspace>,
    selected_workspace_id: Option<String>,
    invalid_reason: Option<String>,
}

impl ChatGptSessionStore for ChatGptVault {
    fn load(&self) -> Result<Option<ChatGptSession>, ChatGptApiError> {
        let persisted: Option<PersistedChatGptSession> =
            load_json(&self.store, &self.key).map_err(chatgpt_store_error)?;
        persisted
            .map(|persisted| {
                let tokens = OAuthTokenSet::new(
                    persisted.access_token,
                    persisted.refresh_token,
                    persisted.expires_at,
                )
                .map_err(|_| chatgpt_store_error(CredentialError::CorruptCredential))?
                .with_identity_token(persisted.identity_token)
                .map_err(|_| chatgpt_store_error(CredentialError::CorruptCredential))?;
                Ok(ChatGptSession {
                    tokens,
                    workspaces: persisted.workspaces,
                    selected_workspace_id: persisted.selected_workspace_id,
                    invalid_reason: persisted.invalid_reason,
                })
            })
            .transpose()
    }

    fn save(&self, session: &ChatGptSession) -> Result<(), ChatGptApiError> {
        save_json(
            &self.store,
            &self.key,
            &PersistedChatGptSession {
                access_token: session.tokens.access_token().into(),
                refresh_token: session.tokens.refresh_token().map(str::to_owned),
                identity_token: session.tokens.identity_token().map(str::to_owned),
                expires_at: session.tokens.expires_at,
                workspaces: session.workspaces.clone(),
                selected_workspace_id: session.selected_workspace_id.clone(),
                invalid_reason: session.invalid_reason.clone(),
            },
        )
        .map_err(chatgpt_store_error)
    }

    fn clear(&self) -> Result<(), ChatGptApiError> {
        delete(&self.store, &self.key).map_err(chatgpt_store_error)
    }
}

fn load_json<T: for<'de> Deserialize<'de>>(
    store: &CredentialStore,
    key: &CredentialKey,
) -> Result<Option<T>, CredentialError> {
    let stored = match store.get(key) {
        Ok(stored) => stored,
        Err(CredentialError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let payload = stored
        .credential()
        .get("session")
        .ok_or(CredentialError::CorruptCredential)?;
    serde_json::from_slice(payload.expose())
        .map(Some)
        .map_err(|_| CredentialError::CorruptCredential)
}

fn save_json<T: Serialize>(
    store: &CredentialStore,
    key: &CredentialKey,
    value: &T,
) -> Result<(), CredentialError> {
    let encoded = serde_json::to_vec(value).map_err(|_| CredentialError::CorruptCredential)?;
    let mut credential = Credential::new();
    credential.insert("session", SecretValue::new(encoded))?;
    store.set(key, credential)?;
    Ok(())
}

fn delete(store: &CredentialStore, key: &CredentialKey) -> Result<(), CredentialError> {
    match store.delete(key) {
        Ok(()) | Err(CredentialError::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

fn provider_store_error(error: CredentialError) -> ullage_core::ProviderError {
    ullage_core::ProviderError::ProtocolIncompatible {
        message: error.provider_message("credential store operation failed"),
    }
}

fn chatgpt_store_error(error: CredentialError) -> ChatGptApiError {
    ChatGptApiError::new(
        ChatGptApiErrorKind::ProtocolIncompatible,
        error.provider_message("credential store operation failed"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chrono::Utc;
    use ullage_auth::{
        Availability, BackendKind, BackendScope, CredentialBackend, CredentialError,
    };

    use super::*;

    #[derive(Default)]
    struct MemoryBackend {
        values: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl CredentialBackend for MemoryBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::ExplicitFileFallback
        }

        fn coordination_scope(&self) -> BackendScope {
            BackendScope::new(b"ullage-app-credential-test")
        }

        fn probe(&self) -> Result<Availability, CredentialError> {
            Ok(Availability::Available)
        }

        fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
            let identity = format!("{}:{}", key.service_name(), key.entry_name());
            self.values
                .lock()
                .map_err(|_| CredentialError::Synchronization)?
                .get(&identity)
                .cloned()
                .ok_or(CredentialError::NotFound)
        }

        fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
            let identity = format!("{}:{}", key.service_name(), key.entry_name());
            self.values
                .lock()
                .map_err(|_| CredentialError::Synchronization)?
                .insert(identity, value.to_vec());
            Ok(())
        }
    }

    #[test]
    fn provider_vaults_round_trip_without_crossing_keys() {
        let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
        let claude = ClaudeVault::new(store.clone(), "claude-a").unwrap();
        let other_claude = ClaudeVault::new(store.clone(), "claude-b").unwrap();
        let chatgpt = ChatGptVault::new(store, "chatgpt-a").unwrap();
        let claude_credential = ClaudeCredential {
            access_token: "claude-access".into(),
            refresh_token: Some("claude-refresh".into()),
            expires_at: Some(Utc::now()),
            account_label: None,
            account_key: None,
        };
        claude.save(&claude_credential).unwrap();
        assert_eq!(claude.load().unwrap(), Some(claude_credential));
        assert!(other_claude.load().unwrap().is_none());
        assert!(chatgpt.load().unwrap().is_none());

        let tokens =
            OAuthTokenSet::new("chatgpt-access", Some("chatgpt-refresh".into()), None).unwrap();
        let session = ChatGptSession {
            tokens,
            workspaces: vec![ChatGptWorkspace {
                id: "workspace-a".into(),
                label: Some("A".into()),
            }],
            selected_workspace_id: Some("workspace-a".into()),
            invalid_reason: None,
        };
        chatgpt.save(&session).unwrap();
        let loaded = chatgpt.load().unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token(), "chatgpt-access");
        assert_eq!(loaded.selected_workspace_id.as_deref(), Some("workspace-a"));
        assert!(claude.load().unwrap().is_some());
        assert!(other_claude.load().unwrap().is_none());
    }

    #[test]
    fn claude_vault_rejects_semantically_corrupt_stored_tokens() {
        let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
        let vault = ClaudeVault::new(store.clone(), "claude-corrupt").unwrap();
        for credential in [
            ClaudeCredential {
                access_token: " ".into(),
                refresh_token: Some("refresh".into()),
                expires_at: None,
                account_label: None,
                account_key: None,
            },
            ClaudeCredential {
                access_token: "access".into(),
                refresh_token: None,
                expires_at: None,
                account_label: None,
                account_key: None,
            },
            ClaudeCredential {
                access_token: "access".into(),
                refresh_token: Some(" ".into()),
                expires_at: None,
                account_label: None,
                account_key: None,
            },
        ] {
            save_json(&store, &vault.key, &credential).unwrap();
            assert!(matches!(
                vault.load(),
                Err(ullage_core::ProviderError::ProtocolIncompatible { .. })
            ));
        }
    }

    #[test]
    fn file_fallback_disabled_is_visible_in_provider_errors() {
        match provider_store_error(CredentialError::FileFallbackDisabled) {
            ullage_core::ProviderError::ProtocolIncompatible { message } => {
                assert!(message.contains("Secret Service"));
                assert!(message.contains("file_fallback"));
            }
            other => panic!("unexpected error {other:?}"),
        }
        assert!(
            chatgpt_store_error(CredentialError::FileFallbackDisabled)
                .message
                .contains("Secret Service")
        );
        assert_eq!(
            provider_store_error(CredentialError::BackendUnavailable).to_string(),
            "provider protocol is incompatible: credential store operation failed"
        );
    }

    #[test]
    fn chatgpt_vault_rejects_whitespace_tokens() {
        let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
        let vault = ChatGptVault::new(store.clone(), "chatgpt-corrupt").unwrap();
        for (access_token, refresh_token, identity_token) in [
            (" ", Some("refresh"), Some("identity")),
            ("access", Some(" "), Some("identity")),
            ("access", Some("refresh"), Some(" ")),
        ] {
            save_json(
                &store,
                &vault.key,
                &PersistedChatGptSession {
                    access_token: access_token.into(),
                    refresh_token: refresh_token.map(str::to_owned),
                    identity_token: identity_token.map(str::to_owned),
                    expires_at: None,
                    workspaces: Vec::new(),
                    selected_workspace_id: None,
                    invalid_reason: None,
                },
            )
            .unwrap();
            assert!(vault.load().is_err());
        }
    }
}
