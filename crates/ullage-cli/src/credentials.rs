use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ullage_auth::{
    Credential, CredentialError, CredentialKey, CredentialStore, CredentialVersion, ReplaceOutcome,
    SecretValue,
};
use ullage_provider_chatgpt::{
    ChatGptApiError, ChatGptApiErrorKind, ChatGptSession, ChatGptSessionStore,
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
    fn load(&self) -> ullage_core::ProviderResult<Option<(ClaudeCredential, CredentialVersion)>> {
        let loaded: Option<(ClaudeCredential, CredentialVersion)> =
            load_json(&self.store, &self.key).map_err(provider_store_error)?;
        loaded
            .map(|(credential, version)| {
                credential.validate()?;
                Ok((credential, version))
            })
            .transpose()
    }

    fn save(
        &self,
        credential: &ClaudeCredential,
    ) -> ullage_core::ProviderResult<CredentialVersion> {
        save_json(&self.store, &self.key, credential).map_err(provider_store_error)
    }

    fn replace(
        &self,
        expected: CredentialVersion,
        credential: &ClaudeCredential,
    ) -> ullage_core::ProviderResult<Option<CredentialVersion>> {
        replace_json(&self.store, &self.key, expected, credential).map_err(provider_store_error)
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

// ChatGptSession itself is the persisted shape: its Deserialize runs every
// token back through OAuthTokenSet validation, so a tampered store record is
// rejected by the same rules as a fresh OAuth response.
impl ChatGptSessionStore for ChatGptVault {
    fn load(&self) -> Result<Option<(ChatGptSession, CredentialVersion)>, ChatGptApiError> {
        load_json(&self.store, &self.key).map_err(chatgpt_store_error)
    }

    fn save(&self, session: &ChatGptSession) -> Result<CredentialVersion, ChatGptApiError> {
        save_json(&self.store, &self.key, session).map_err(chatgpt_store_error)
    }

    fn replace(
        &self,
        expected: CredentialVersion,
        session: &ChatGptSession,
    ) -> Result<Option<CredentialVersion>, ChatGptApiError> {
        replace_json(&self.store, &self.key, expected, session).map_err(chatgpt_store_error)
    }

    fn clear(&self) -> Result<(), ChatGptApiError> {
        delete(&self.store, &self.key).map_err(chatgpt_store_error)
    }
}

/// Loads the decoded value together with the version it was observed at; the
/// version is what `replace_json` needs to write it back safely.
fn load_json<T: for<'de> Deserialize<'de>>(
    store: &CredentialStore,
    key: &CredentialKey,
) -> Result<Option<(T, CredentialVersion)>, CredentialError> {
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
        .map(|value| Some((value, stored.version())))
        .map_err(|_| CredentialError::CorruptCredential)
}

/// Unconditional write: first sign-in or an explicit re-login only. Returns
/// the version the record now carries.
fn save_json<T: Serialize>(
    store: &CredentialStore,
    key: &CredentialKey,
    value: &T,
) -> Result<CredentialVersion, CredentialError> {
    let encoded = serde_json::to_vec(value).map_err(|_| CredentialError::CorruptCredential)?;
    let mut credential = Credential::new();
    credential.insert("session", SecretValue::new(encoded))?;
    Ok(store.set(key, credential)?.version())
}

/// Compare-and-swap write for a value derived from a `load_json`. `Ok(None)`
/// covers both a version conflict and a record that was deleted meanwhile: in
/// either case the observed session is stale and nothing was written.
fn replace_json<T: Serialize>(
    store: &CredentialStore,
    key: &CredentialKey,
    expected: CredentialVersion,
    value: &T,
) -> Result<Option<CredentialVersion>, CredentialError> {
    let encoded = serde_json::to_vec(value).map_err(|_| CredentialError::CorruptCredential)?;
    let mut credential = Credential::new();
    credential.insert("session", SecretValue::new(encoded))?;
    match store.replace(key, expected, credential) {
        Ok(ReplaceOutcome::Replaced(stored)) => Ok(Some(stored.version())),
        Ok(ReplaceOutcome::VersionConflict) | Err(CredentialError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
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
    use ullage_provider_chatgpt::{ChatGptWorkspace, OAuthTokenSet};

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

    fn claude_credential(access_token: &str) -> ClaudeCredential {
        ClaudeCredential {
            access_token: access_token.into(),
            refresh_token: Some("claude-refresh".into()),
            expires_at: Some(Utc::now()),
            account_label: None,
            account_key: None,
        }
    }

    #[test]
    fn provider_vaults_round_trip_without_crossing_keys() {
        let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
        let claude = ClaudeVault::new(store.clone(), "claude-a").unwrap();
        let other_claude = ClaudeVault::new(store.clone(), "claude-b").unwrap();
        let chatgpt = ChatGptVault::new(store, "chatgpt-a").unwrap();
        let claude_credential = claude_credential("claude-access");
        claude.save(&claude_credential).unwrap();
        assert_eq!(
            claude.load().unwrap().map(|(credential, _)| credential),
            Some(claude_credential)
        );
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
        let (loaded, _) = chatgpt.load().unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token(), "chatgpt-access");
        assert_eq!(loaded.selected_workspace_id.as_deref(), Some("workspace-a"));
        assert!(claude.load().unwrap().is_some());
        assert!(other_claude.load().unwrap().is_none());
    }

    #[test]
    fn claude_vault_stale_replace_never_overwrites_or_resurrects() {
        let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
        let vault = ClaudeVault::new(store.clone(), "claude-stale").unwrap();
        let (_, first_version) = {
            vault.save(&claude_credential("first")).unwrap();
            vault.load().unwrap().unwrap()
        };

        // A delete tombstones the key: a stale replace must not resurrect it.
        vault.clear().unwrap();
        assert_eq!(
            vault
                .replace(first_version, &claude_credential("stale"))
                .unwrap(),
            None
        );
        assert!(vault.load().unwrap().is_none());

        // A new login legitimately recreates the record at a fresh generation.
        vault.save(&claude_credential("second")).unwrap();
        let (_, second_version) = vault.load().unwrap().unwrap();
        assert_ne!(first_version, second_version);
        assert_eq!(
            vault
                .replace(first_version, &claude_credential("stale"))
                .unwrap(),
            None
        );
        assert_eq!(
            vault
                .load()
                .unwrap()
                .map(|(credential, _)| credential.access_token),
            Some("second".to_owned())
        );
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
            ("access\nforged: yes", Some("refresh"), Some("identity")),
        ] {
            save_json(
                &store,
                &vault.key,
                &serde_json::json!({
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "identity_token": identity_token,
                    "expires_at": null,
                    "workspaces": [],
                    "selected_workspace_id": null,
                    "invalid_reason": null,
                }),
            )
            .unwrap();
            assert!(vault.load().is_err());
        }
    }
}
