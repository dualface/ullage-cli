use std::path::PathBuf;
use std::sync::Arc;

use ullage_auth::{
    Availability, BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialKey,
    CredentialStore, FileFallbackOptions, FileStore, NativeStore,
};
use ullage_protocol::CredentialBackendId;

use crate::config::CredentialsSettings;

struct UnavailableNativeBackend {
    kind: BackendKind,
    scope: BackendScope,
}

impl CredentialBackend for UnavailableNativeBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn coordination_scope(&self) -> BackendScope {
        self.scope
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Unavailable)
    }

    fn read(&self, _: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        Err(CredentialError::FileFallbackDisabled)
    }

    fn write(&self, _: &CredentialKey, _: &[u8]) -> Result<(), CredentialError> {
        Err(CredentialError::FileFallbackDisabled)
    }
}

pub(crate) fn assemble_credential_store(
    settings: &CredentialsSettings,
) -> Result<(Arc<CredentialStore>, CredentialBackendId), String> {
    assemble_credential_store_with(settings.file_fallback, NativeStore::new(), || {
        FileStore::new(FileFallbackOptions::new(default_credentials_dir()?).map_err(store_error)?)
            .map_err(store_error)
    })
}

pub(crate) fn assemble_credential_store_with(
    file_fallback: bool,
    native: impl CredentialBackend + 'static,
    open_fallback: impl FnOnce() -> Result<FileStore, String>,
) -> Result<(Arc<CredentialStore>, CredentialBackendId), String> {
    match native.probe() {
        Ok(Availability::Available) => {
            let backend = CredentialBackendId::from(native.kind());
            Ok((Arc::new(CredentialStore::new(native)), backend))
        }
        Ok(Availability::Unavailable) if file_fallback => {
            let store = open_fallback()?;
            let backend = CredentialBackendId::from(store.kind());
            Ok((Arc::new(CredentialStore::new(store)), backend))
        }
        Ok(Availability::Unavailable) => {
            let backend = CredentialBackendId::from(native.kind());
            Ok((
                Arc::new(CredentialStore::new(UnavailableNativeBackend {
                    kind: native.kind(),
                    scope: native.coordination_scope(),
                })),
                backend,
            ))
        }
        Err(_) => {
            let backend = CredentialBackendId::from(native.kind());
            Ok((Arc::new(CredentialStore::new(native)), backend))
        }
    }
}

pub(crate) fn default_credentials_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join("Library/Application Support/Ullage/credentials"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(root).join("ullage/credentials"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home).join(".local/share/ullage/credentials"));
        }
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(root).join("Ullage/credentials"));
    }
    Err("credential fallback path unavailable".into())
}

fn store_error(error: CredentialError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ullage_auth::{Credential, SecretValue};

    struct ProbeBackend {
        kind: BackendKind,
        availability: Result<Availability, CredentialError>,
    }

    impl CredentialBackend for ProbeBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn coordination_scope(&self) -> BackendScope {
            BackendScope::new(b"ullage-app-assemble-test")
        }

        fn probe(&self) -> Result<Availability, CredentialError> {
            self.availability.clone()
        }

        fn read(&self, _: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
            Err(CredentialError::NotFound)
        }

        fn write(&self, _: &CredentialKey, _: &[u8]) -> Result<(), CredentialError> {
            Ok(())
        }
    }

    fn available_native() -> ProbeBackend {
        ProbeBackend {
            kind: BackendKind::LinuxSecretService,
            availability: Ok(Availability::Available),
        }
    }

    fn unavailable_native() -> ProbeBackend {
        ProbeBackend {
            kind: BackendKind::LinuxSecretService,
            availability: Ok(Availability::Unavailable),
        }
    }

    fn panic_fallback() -> Result<FileStore, String> {
        panic!("file store must not be constructed when the native backend is used")
    }

    fn sample_credential() -> Credential {
        let mut credential = Credential::new();
        credential
            .insert("session", SecretValue::new(b"secret".to_vec()))
            .unwrap();
        credential
    }

    #[test]
    fn available_native_backend_does_not_construct_file_store() {
        for file_fallback in [false, true] {
            let (store, backend) =
                assemble_credential_store_with(file_fallback, available_native(), panic_fallback)
                    .unwrap();
            assert_eq!(backend, CredentialBackendId::LinuxSecretService);
            assert_eq!(store.backend_kind(), BackendKind::LinuxSecretService);
        }
    }

    #[test]
    fn enabled_fallback_constructs_file_store_when_native_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("credentials");
        let (store, backend) = assemble_credential_store_with(true, unavailable_native(), || {
            FileStore::new(FileFallbackOptions::new(&root).unwrap()).map_err(store_error)
        })
        .unwrap();
        assert_eq!(backend, CredentialBackendId::FileFallback);
        assert_eq!(store.backend_kind(), BackendKind::ExplicitFileFallback);

        let key = CredentialKey::new("claude", "account-a").unwrap();
        store.set(&key, sample_credential()).unwrap();
        let loaded = store.get(&key).unwrap();
        assert_eq!(
            loaded.credential().get("session").unwrap().expose(),
            b"secret"
        );
    }

    #[test]
    fn disabled_fallback_returns_a_dedicated_error_when_native_is_unavailable() {
        let (store, backend) =
            assemble_credential_store_with(false, unavailable_native(), panic_fallback).unwrap();
        assert_eq!(backend, CredentialBackendId::LinuxSecretService);
        let key = CredentialKey::new("claude", "account-a").unwrap();
        let error = store.set(&key, sample_credential()).unwrap_err();
        assert_eq!(error, CredentialError::FileFallbackDisabled);
        assert!(error.to_string().contains("Secret Service"));
        assert!(error.to_string().contains("file_fallback"));
    }

    #[test]
    fn file_store_construction_failure_does_not_downgrade_to_native() {
        let error = assemble_credential_store_with(true, unavailable_native(), || {
            Err("fallback directory is not private".into())
        })
        .unwrap_err();
        assert_eq!(error, "fallback directory is not private");
    }

    #[test]
    fn default_credentials_dir_is_an_absolute_platform_path() {
        let path = default_credentials_dir().unwrap();
        assert!(path.is_absolute());
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("credentials")
        );
    }
}
