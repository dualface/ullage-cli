use keyring::{Entry, Error as KeyringError};
#[cfg(any(windows, test))]
use sha2::{Digest, Sha256};

#[cfg(windows)]
use crate::credential::NAMESPACE;
use crate::{
    Availability, BackendKind, BackendScope, CredentialBackend, CredentialError, CredentialKey,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeStore;

impl NativeStore {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn entry(key: &CredentialKey) -> Result<Entry, CredentialError> {
        if !cfg!(any(
            target_os = "macos",
            target_os = "windows",
            target_os = "linux"
        )) {
            return Err(CredentialError::BackendUnavailable);
        }
        #[cfg(windows)]
        let entry = Entry::new_with_target(&target_name(key), NAMESPACE, "ullage");
        #[cfg(not(windows))]
        let entry = Entry::new(&key.service_name(), key.entry_name());
        entry.map_err(map_keyring_error)
    }
}

impl CredentialBackend for NativeStore {
    fn kind(&self) -> BackendKind {
        if cfg!(target_os = "macos") {
            BackendKind::MacOsKeychain
        } else if cfg!(target_os = "windows") {
            BackendKind::WindowsCredentialManager
        } else if cfg!(target_os = "linux") {
            BackendKind::LinuxSecretService
        } else {
            BackendKind::OtherPlatform
        }
    }

    fn coordination_scope(&self) -> BackendScope {
        BackendScope::new(b"ullage-native-credential-vault")
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        let key = CredentialKey::new("availability-probe", "ullage-read-only-probe")?;
        let entry = match Self::entry(&key) {
            Ok(entry) => entry,
            // On platforms with no native backend, probing reports
            // unavailability so a configured file fallback can take over.
            Err(CredentialError::BackendUnavailable) => return Ok(Availability::Unavailable),
            Err(error) => return Err(error),
        };
        match entry.get_secret() {
            Ok(mut secret) => {
                use zeroize::Zeroize;
                secret.zeroize();
                Ok(Availability::Available)
            }
            Err(KeyringError::NoEntry) => Ok(Availability::Available),
            Err(error) if backend_is_unavailable(&error) => Ok(Availability::Unavailable),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        Self::entry(key)?.get_secret().map_err(map_keyring_error)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        Self::entry(key)?
            .set_secret(value)
            .map_err(map_keyring_error)
    }
}

#[cfg(any(windows, test))]
pub(crate) fn target_name(key: &CredentialKey) -> String {
    format!(
        "ullage:{}",
        crate::credential::hex(&Sha256::digest(key.stable_bytes()))
    )
}

#[cfg(target_os = "linux")]
fn backend_is_unavailable(error: &KeyringError) -> bool {
    if let KeyringError::PlatformFailure(source) = error {
        return source
            .downcast_ref::<dbus_secret_service::Error>()
            .is_some_and(|error| match error {
                dbus_secret_service::Error::Dbus(error) => matches!(
                    error.name(),
                    Some(
                        "org.freedesktop.DBus.Error.ServiceUnknown"
                            | "org.freedesktop.DBus.Error.NameHasNoOwner"
                            | "org.freedesktop.DBus.Error.NoServer"
                            | "org.freedesktop.DBus.Error.Disconnected"
                            | "org.freedesktop.DBus.Error.NotSupported"
                            | "org.freedesktop.DBus.Error.FileNotFound"
                    )
                ),
                _ => false,
            });
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn backend_is_unavailable(_error: &KeyringError) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn backend_is_access_denied(error: &KeyringError) -> bool {
    if let KeyringError::PlatformFailure(source) = error {
        return source
            .downcast_ref::<dbus_secret_service::Error>()
            .is_some_and(|error| match error {
                dbus_secret_service::Error::Dbus(error) => {
                    error.name() == Some("org.freedesktop.DBus.Error.AccessDenied")
                }
                _ => false,
            });
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn backend_is_access_denied(_error: &KeyringError) -> bool {
    false
}

pub(crate) fn map_keyring_error(error: KeyringError) -> CredentialError {
    if backend_is_unavailable(&error) {
        return CredentialError::BackendUnavailable;
    }
    if backend_is_access_denied(&error) {
        return CredentialError::AccessDenied;
    }
    match error {
        KeyringError::NoEntry => CredentialError::NotFound,
        KeyringError::NoStorageAccess(_) => CredentialError::AccessDenied,
        KeyringError::TooLong(_, _) => CredentialError::CredentialTooLarge,
        KeyringError::BadEncoding(_) => CredentialError::CorruptCredential,
        _ => CredentialError::BackendFailure,
    }
}
