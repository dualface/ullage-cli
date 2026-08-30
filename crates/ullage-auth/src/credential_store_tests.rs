use std::collections::HashMap;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier as ThreadBarrier, Mutex};
use std::time::Duration;

use tokio::sync::Barrier;

use crate::{
    Availability, BackendKind, BackendScope, Credential, CredentialBackend, CredentialError,
    CredentialKey, CredentialStore, FileFallbackOptions, FileStore, NativeStore, RefreshError,
    RefreshFailure, RefreshFailureKind, ReplaceOutcome, SecretValue,
};

#[derive(Clone)]
struct MemoryBackend {
    values: Arc<Mutex<HashMap<CredentialKey, Vec<u8>>>>,
    writes: Arc<AtomicUsize>,
    scope: BackendScope,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        static NEXT_SCOPE: AtomicUsize = AtomicUsize::new(0);
        let mut identity = b"memory-backend\0".to_vec();
        identity.extend_from_slice(&NEXT_SCOPE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        Self {
            values: Arc::new(Mutex::new(HashMap::new())),
            writes: Arc::new(AtomicUsize::new(0)),
            scope: BackendScope::new(&identity),
        }
    }
}

struct ToggleBackendState {
    values: Mutex<HashMap<CredentialKey, Vec<u8>>>,
    fail_reads: AtomicBool,
    reads: AtomicUsize,
    read_gate: Mutex<Option<(Arc<ThreadBarrier>, Arc<ThreadBarrier>)>>,
    write_gate: Mutex<Option<(Arc<ThreadBarrier>, Arc<ThreadBarrier>)>>,
    scope: BackendScope,
}

impl Default for ToggleBackendState {
    fn default() -> Self {
        static NEXT_SCOPE: AtomicUsize = AtomicUsize::new(0);
        let mut identity = b"toggle-backend\0".to_vec();
        identity.extend_from_slice(&NEXT_SCOPE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        Self {
            values: Mutex::new(HashMap::new()),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            read_gate: Mutex::new(None),
            write_gate: Mutex::new(None),
            scope: BackendScope::new(&identity),
        }
    }
}

struct ToggleBackend {
    state: Arc<ToggleBackendState>,
}

impl CredentialBackend for ToggleBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OtherPlatform
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }

    fn coordination_scope(&self) -> BackendScope {
        self.state.scope
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        let read_index = self.state.reads.fetch_add(1, Ordering::SeqCst);
        if self.state.fail_reads.load(Ordering::SeqCst) {
            if read_index == 0 {
                if let Some((entered, release)) = self
                    .state
                    .read_gate
                    .lock()
                    .map_err(|_| CredentialError::Synchronization)?
                    .clone()
                {
                    entered.wait();
                    release.wait();
                }
            }
            return Err(CredentialError::BackendUnavailable);
        }
        self.state
            .values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .get(key)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        if let Some((entered, release)) = self
            .state
            .write_gate
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .clone()
        {
            entered.wait();
            release.wait();
        }
        self.state
            .values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .insert(key.clone(), value.to_vec());
        Ok(())
    }
}

impl CredentialBackend for MemoryBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OtherPlatform
    }

    fn probe(&self) -> Result<Availability, CredentialError> {
        Ok(Availability::Available)
    }

    fn coordination_scope(&self) -> BackendScope {
        self.scope
    }

    fn read(&self, key: &CredentialKey) -> Result<Vec<u8>, CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .get(key)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }

    fn write(&self, key: &CredentialKey, value: &[u8]) -> Result<(), CredentialError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.values
            .lock()
            .map_err(|_| CredentialError::Synchronization)?
            .insert(key.clone(), value.to_vec());
        Ok(())
    }
}

fn credential(token: &[u8]) -> Credential {
    let mut credential = Credential::new();
    credential
        .insert("refresh_token", SecretValue::new(token))
        .unwrap();
    credential
}

#[test]
fn key_is_stable_and_account_scoped() {
    let first = CredentialKey::new("chatgpt", "account-a").unwrap();
    let second = CredentialKey::new("chatgpt", "account-b").unwrap();
    assert_eq!(
        first.service_name(),
        "dev.onevoke.ullage.credentials.chatgpt"
    );
    assert_eq!(first.entry_name(), "account-a");
    assert_ne!(first.stable_bytes(), second.stable_bytes());
}

#[test]
fn native_target_does_not_alias_delimited_provider_and_account_pairs() {
    let first = CredentialKey::new("left.dev.onevoke.ullage.credentials.right", "acct").unwrap();
    let second = CredentialKey::new("right", "acct.dev.onevoke.ullage.credentials.left").unwrap();
    assert_ne!(
        crate::native_store::target_name(&first),
        crate::native_store::target_name(&second)
    );
    assert_eq!(crate::native_store::target_name(&first).len(), 71);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_secret_service_dbus_unavailability_is_actionable() {
    for name in [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NoServer",
    ] {
        let error = keyring::Error::PlatformFailure(Box::new(dbus_secret_service::Error::Dbus(
            dbus::Error::new_custom(name, "unavailable"),
        )));
        assert_eq!(
            crate::native_store::map_keyring_error(error),
            CredentialError::BackendUnavailable
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_secret_service_dbus_access_denied_is_actionable() {
    let error = keyring::Error::PlatformFailure(Box::new(dbus_secret_service::Error::Dbus(
        dbus::Error::new_custom("org.freedesktop.DBus.Error.AccessDenied", "denied"),
    )));
    let mapped = crate::native_store::map_keyring_error(error);
    assert_eq!(mapped, CredentialError::AccessDenied);
    let message = mapped.to_string();
    assert!(message.contains("unlock"));
    assert!(message.contains("permissions"));
}

#[test]
fn native_storage_access_failure_has_combined_remediation() {
    let error = keyring::Error::NoStorageAccess(Box::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "denied",
    )));
    let mapped = crate::native_store::map_keyring_error(error);
    assert_eq!(mapped, CredentialError::AccessDenied);
    let message = mapped.to_string();
    assert!(message.contains("unlock"));
    assert!(message.contains("permissions"));
}

#[cfg(windows)]
#[test]
fn windows_native_entry_accepts_maximum_length_key_components() {
    let key = CredentialKey::new("p".repeat(255), "a".repeat(255)).unwrap();
    assert!(crate::native_store::NativeStore::entry(&key).is_ok());
}

#[cfg(windows)]
#[test]
fn windows_file_reparse_attributes_are_rejected() {
    const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    assert!(crate::file_store::windows_file_attributes_are_safe(
        FILE_ATTRIBUTE_ARCHIVE
    ));
    assert!(!crate::file_store::windows_file_attributes_are_safe(
        FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_REPARSE_POINT
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_native_entry_uses_the_default_keychain_domain() {
    let key = CredentialKey::new("claude", "macos-constructor").unwrap();
    assert!(crate::native_store::NativeStore::entry(&key).is_ok());
}

#[test]
fn values_and_errors_are_redacted() {
    let secret = SecretValue::new(b"top-secret".to_vec());
    let mut value = Credential::new();
    value.insert("access_token", secret).unwrap();
    let rendered = format!("{value:?}");
    assert!(!rendered.contains("top-secret"));
    assert!(rendered.contains("REDACTED"));

    let refresh_error = RefreshError::Refresh(RefreshFailure::new(RefreshFailureKind::Network));
    assert!(format!("{refresh_error:?}").contains("REDACTED"));
    assert!(!format!("{refresh_error}").contains("network"));
}

#[test]
fn accounts_are_isolated_and_replace_is_compare_and_swap() {
    let store = CredentialStore::new(MemoryBackend::default());
    let first_key = CredentialKey::new("claude", "first").unwrap();
    let second_key = CredentialKey::new("claude", "second").unwrap();

    let first = store.set(&first_key, credential(b"first-token")).unwrap();
    store.set(&second_key, credential(b"second-token")).unwrap();
    assert_eq!(
        store
            .get(&first_key)
            .unwrap()
            .credential()
            .get("refresh_token")
            .unwrap()
            .expose(),
        b"first-token"
    );
    assert_eq!(
        store
            .replace(&first_key, first.version(), credential(b"rotated"))
            .unwrap(),
        ReplaceOutcome::Replaced
    );
    assert_eq!(
        store
            .replace(&first_key, first.version(), credential(b"stale"))
            .unwrap(),
        ReplaceOutcome::VersionConflict
    );
    assert_eq!(
        store
            .get(&second_key)
            .unwrap()
            .credential()
            .get("refresh_token")
            .unwrap()
            .expose(),
        b"second-token"
    );
    store.delete(&second_key).unwrap();
    assert_eq!(
        store.get(&second_key).unwrap_err(),
        CredentialError::NotFound
    );
}

#[test]
fn get_waits_for_same_account_write() {
    let state = Arc::new(ToggleBackendState::default());
    let store = Arc::new(CredentialStore::new(ToggleBackend {
        state: Arc::clone(&state),
    }));
    let key = CredentialKey::new("claude", "read-write-serialized").unwrap();
    store.set(&key, credential(b"initial")).unwrap();
    state.reads.store(0, Ordering::SeqCst);
    let write_entered = Arc::new(ThreadBarrier::new(2));
    let release_write = Arc::new(ThreadBarrier::new(2));
    *state.write_gate.lock().unwrap() =
        Some((Arc::clone(&write_entered), Arc::clone(&release_write)));

    let writer = {
        let store = Arc::clone(&store);
        let key = key.clone();
        std::thread::spawn(move || store.set(&key, credential(b"updated")))
    };
    write_entered.wait();
    let reads_before_get = state.reads.load(Ordering::SeqCst);
    let reader = {
        let store = Arc::clone(&store);
        let key = key.clone();
        std::thread::spawn(move || store.get(&key))
    };
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(state.reads.load(Ordering::SeqCst), reads_before_get);

    release_write.wait();
    writer.join().unwrap().unwrap();
    let read = reader.join().unwrap().unwrap();
    assert_eq!(
        read.credential().get("refresh_token").unwrap().expose(),
        b"updated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_is_single_flight_for_one_account() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("grok", "shared").unwrap();
    let initial = store.set(&key, credential(b"old")).unwrap();
    let initial_version = initial.version();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();

    for _ in 0..8 {
        let store = Arc::clone(&store);
        let key = key.clone();
        let calls = Arc::clone(&calls);
        tasks.push(tokio::spawn(async move {
            store
                .refresh(&key, initial_version, move |_| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(credential(b"rotated"))
                })
                .await
                .unwrap()
        }));
    }

    for task in tasks {
        assert_eq!(task.await.unwrap().revision(), 2);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_is_single_flight_across_store_handles() {
    let backend = MemoryBackend::default();
    let first_store = Arc::new(CredentialStore::new(backend.clone()));
    let second_store = Arc::new(CredentialStore::new(backend));
    let key = CredentialKey::new("grok", "shared-handles").unwrap();
    let initial = first_store.set(&key, credential(b"old")).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(9));
    let mut tasks = Vec::new();

    for index in 0..8 {
        let store = if index % 2 == 0 {
            Arc::clone(&first_store)
        } else {
            Arc::clone(&second_store)
        };
        let key = key.clone();
        let calls = Arc::clone(&calls);
        let start = Arc::clone(&start);
        let version = initial.version();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            store
                .refresh(&key, version, move |_| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(credential(b"rotated"))
                })
                .await
                .unwrap()
        }));
    }
    start.wait().await;

    for task in tasks {
        assert_eq!(task.await.unwrap().revision(), 2);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_versions_refresh_serially_for_one_account() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("grok", "version-serialized").unwrap();
    let first = store.set(&key, credential(b"version-one")).unwrap();
    let first_started = Arc::new(tokio::sync::Notify::new());
    let release_first = Arc::new(tokio::sync::Notify::new());
    let first_started_wait = first_started.notified();

    let first_task = {
        let store = Arc::clone(&store);
        let key = key.clone();
        let first_started = Arc::clone(&first_started);
        let release_first = Arc::clone(&release_first);
        tokio::spawn(async move {
            store
                .refresh(&key, first.version(), move |_| async move {
                    first_started.notify_one();
                    release_first.notified().await;
                    Ok(credential(b"stale-provider-result"))
                })
                .await
        })
    };
    first_started_wait.await;

    let second = store.set(&key, credential(b"version-two")).unwrap();
    let second_version = second.version();
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_task = {
        let store = Arc::clone(&store);
        let key = key.clone();
        let second_calls = Arc::clone(&second_calls);
        tokio::spawn(async move {
            store
                .refresh(&key, second_version, move |_| async move {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(credential(b"version-three"))
                })
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    release_first.notify_one();
    assert_eq!(first_task.await.unwrap().unwrap().version(), second_version);
    assert_eq!(second_task.await.unwrap().unwrap().revision(), 3);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_refresh_result_is_shared_by_all_waiters() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("grok", "failed-flight").unwrap();
    let initial = store.set(&key, credential(b"old")).unwrap();
    let initial_version = initial.version();
    let calls = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(9));
    let mut tasks = Vec::new();

    for _ in 0..8 {
        let store = Arc::clone(&store);
        let key = key.clone();
        let calls = Arc::clone(&calls);
        let start = Arc::clone(&start);
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            store
                .refresh(&key, initial_version, move |_| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Err(RefreshFailure::new(RefreshFailureKind::Network))
                })
                .await
        }));
    }
    start.wait().await;

    for task in tasks {
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            RefreshError::Refresh(RefreshFailure::new(RefreshFailureKind::Network))
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 10)]
async fn backend_read_failure_is_shared_by_all_waiters() {
    let state = Arc::new(ToggleBackendState::default());
    let store = Arc::new(CredentialStore::new(ToggleBackend {
        state: Arc::clone(&state),
    }));
    let key = CredentialKey::new("grok", "failed-read-flight").unwrap();
    let initial = store.set(&key, credential(b"old")).unwrap();
    let initial_version = initial.version();
    state.reads.store(0, Ordering::SeqCst);
    state.fail_reads.store(true, Ordering::SeqCst);
    let entered = Arc::new(ThreadBarrier::new(2));
    let release = Arc::new(ThreadBarrier::new(2));
    *state.read_gate.lock().unwrap() = Some((Arc::clone(&entered), Arc::clone(&release)));
    let mut tasks = Vec::new();

    {
        let store = Arc::clone(&store);
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            store
                .refresh(&key, initial_version, |_| async {
                    Ok(credential(b"must-not-run"))
                })
                .await
        }));
    }
    entered.wait();
    for _ in 0..7 {
        let store = Arc::clone(&store);
        let key = key.clone();
        tasks.push(tokio::spawn(async move {
            store
                .refresh(&key, initial_version, |_| async {
                    Ok(credential(b"must-not-run"))
                })
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    release.wait();

    for task in tasks {
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            RefreshError::Store(CredentialError::BackendUnavailable)
        );
    }
    assert_eq!(state.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_refresh_leader_releases_waiters() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("grok", "cancelled-flight").unwrap();
    let initial = store.set(&key, credential(b"old")).unwrap();
    let initial_version = initial.version();
    let started = Arc::new(tokio::sync::Notify::new());
    let never_finish = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();

    let leader = {
        let store = Arc::clone(&store);
        let key = key.clone();
        let started = Arc::clone(&started);
        let never_finish = Arc::clone(&never_finish);
        tokio::spawn(async move {
            store
                .refresh(&key, initial_version, move |_| async move {
                    started.notify_one();
                    never_finish.notified().await;
                    Ok(credential(b"unreachable"))
                })
                .await
        })
    };
    started_wait.await;

    let follower = {
        let store = Arc::clone(&store);
        let key = key.clone();
        tokio::spawn(async move {
            store
                .refresh(&key, initial_version, |_| async {
                    Ok(credential(b"must-not-run"))
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    leader.abort();

    let result = tokio::time::timeout(Duration::from_secs(1), follower)
        .await
        .expect("waiter must be released when the leader is cancelled")
        .unwrap();
    assert_eq!(
        result.unwrap_err(),
        RefreshError::Store(CredentialError::Synchronization)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_refresh_cannot_overwrite_recreated_credential() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("chatgpt", "recreated").unwrap();
    let initial = store.set(&key, credential(b"old-session")).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();

    let task = {
        let store = Arc::clone(&store);
        let key = key.clone();
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            store
                .refresh(&key, initial.version(), move |_| async move {
                    started.notify_one();
                    release.notified().await;
                    Ok(credential(b"stale-rotation"))
                })
                .await
        })
    };

    started_wait.await;
    store.delete(&key).unwrap();
    let recreated = store.set(&key, credential(b"new-session")).unwrap();
    assert_eq!(recreated.version().generation(), 2);
    release.notify_one();

    let result = task.await.unwrap().unwrap();
    assert_eq!(result.version(), recreated.version());
    assert_eq!(
        result.credential().get("refresh_token").unwrap().expose(),
        b"new-session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_accounts_refresh_concurrently() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let first_key = CredentialKey::new("cursor", "first").unwrap();
    let second_key = CredentialKey::new("cursor", "second").unwrap();
    let first = store.set(&first_key, credential(b"old-1")).unwrap();
    let second = store.set(&second_key, credential(b"old-2")).unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let first_task = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            store
                .refresh(&first_key, first.version(), move |_| async move {
                    barrier.wait().await;
                    Ok(credential(b"new-1"))
                })
                .await
        })
    };
    let second_task = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            store
                .refresh(&second_key, second.version(), move |_| async move {
                    barrier.wait().await;
                    Ok(credential(b"new-2"))
                })
                .await
        })
    };

    tokio::time::timeout(Duration::from_secs(1), async {
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();
    })
    .await
    .expect("different accounts must not share a refresh lock");
}

#[test]
fn concurrent_stale_replacements_have_one_winner() {
    let store = Arc::new(CredentialStore::new(MemoryBackend::default()));
    let key = CredentialKey::new("chatgpt", "race").unwrap();
    let initial = store.set(&key, credential(b"old")).unwrap();
    let initial_version = initial.version();
    let barrier = Arc::new(ThreadBarrier::new(3));
    let mut threads = Vec::new();
    for token in [b"winner-a".as_slice(), b"winner-b".as_slice()] {
        let store = Arc::clone(&store);
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        let token = token.to_vec();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store
                .replace(&key, initial_version, credential(&token))
                .unwrap()
        }));
    }
    barrier.wait();
    let outcomes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReplaceOutcome::Replaced)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReplaceOutcome::VersionConflict)
            .count(),
        1
    );
}

#[test]
fn concurrent_replacements_across_store_handles_have_one_winner() {
    let backend = MemoryBackend::default();
    let first_store = Arc::new(CredentialStore::new(backend.clone()));
    let second_store = Arc::new(CredentialStore::new(backend));
    let key = CredentialKey::new("chatgpt", "handle-race").unwrap();
    let initial = first_store.set(&key, credential(b"old")).unwrap();
    let barrier = Arc::new(ThreadBarrier::new(3));
    let mut threads = Vec::new();

    for (store, token) in [
        (Arc::clone(&first_store), b"winner-a".as_slice()),
        (Arc::clone(&second_store), b"winner-b".as_slice()),
    ] {
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        let version = initial.version();
        let token = token.to_vec();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.replace(&key, version, credential(&token)).unwrap()
        }));
    }
    barrier.wait();

    let outcomes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReplaceOutcome::Replaced)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReplaceOutcome::VersionConflict)
            .count(),
        1
    );
}

#[test]
fn file_fallback_is_explicit_private_and_strictly_parsed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("vault");
    assert!(matches!(
        FileFallbackOptions::new("relative/path"),
        Err(CredentialError::UnsafeFallbackPath)
    ));
    let backend = FileStore::new(FileFallbackOptions::new(&root).unwrap()).unwrap();
    assert_eq!(backend.probe().unwrap(), Availability::Available);
    let key = CredentialKey::new("claude", "file-account").unwrap();
    let store = CredentialStore::new(backend);
    store.set(&key, credential(b"disk-secret")).unwrap();
    let previous = store.get(&key).unwrap();
    assert_eq!(
        store
            .replace(&key, previous.version(), credential(b"disk-rotated"))
            .unwrap(),
        ReplaceOutcome::Replaced
    );
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .credential()
            .get("refresh_token")
            .unwrap()
            .expose(),
        b"disk-rotated"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let entry = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap();
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    store.delete(&key).unwrap();
    assert_eq!(store.get(&key).unwrap_err(), CredentialError::NotFound);
}

#[test]
fn file_fallback_supports_maximum_length_key_components() {
    let temporary = tempfile::tempdir().unwrap();
    let backend =
        FileStore::new(FileFallbackOptions::new(temporary.path().join("vault")).unwrap()).unwrap();
    let store = CredentialStore::new(backend);
    let key = CredentialKey::new("p".repeat(255), "a".repeat(255)).unwrap();
    store.set(&key, credential(b"long-key-secret")).unwrap();
    assert_eq!(
        store
            .get(&key)
            .unwrap()
            .credential()
            .get("refresh_token")
            .unwrap()
            .expose(),
        b"long-key-secret"
    );
}

#[cfg(unix)]
#[test]
fn equivalent_file_vault_paths_share_coordination() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("vault");
    let alias = PathBuf::from(format!("{}//vault", temporary.path().display()));
    let first_backend = FileStore::new(FileFallbackOptions::new(&root).unwrap()).unwrap();
    let second_backend = FileStore::new(FileFallbackOptions::new(alias).unwrap()).unwrap();
    assert_eq!(
        first_backend.coordination_scope(),
        second_backend.coordination_scope()
    );

    let first_store = Arc::new(CredentialStore::new(first_backend));
    let second_store = Arc::new(CredentialStore::new(second_backend));
    let key = CredentialKey::new("claude", "aliased-vault").unwrap();
    let initial = first_store.set(&key, credential(b"old")).unwrap();
    let barrier = Arc::new(ThreadBarrier::new(3));
    let mut threads = Vec::new();
    for store in [first_store, second_store] {
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        let version = initial.version();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store
                .replace(&key, version, credential(b"replacement"))
                .unwrap()
        }));
    }
    barrier.wait();
    let outcomes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReplaceOutcome::Replaced)
            .count(),
        1
    );
}

#[test]
fn malformed_backend_data_is_rejected_without_secret_output() {
    let backend = MemoryBackend::default();
    let key = CredentialKey::new("claude", "malformed").unwrap();
    backend.write(&key, b"not a credential").unwrap();
    let store = CredentialStore::new(backend);
    let error = store.get(&key).unwrap_err();
    assert_eq!(error, CredentialError::CorruptCredential);
    assert!(!format!("{error:?}").contains("not a credential"));
}

#[test]
fn oversized_credential_is_rejected_before_backend_write() {
    let backend = MemoryBackend::default();
    let writes = Arc::clone(&backend.writes);
    let key = CredentialKey::new("claude", "oversized").unwrap();
    let oversized = credential(&vec![0_u8; 1024 * 1024 + 1]);
    let store = CredentialStore::new(backend);
    assert_eq!(
        store.set(&key, oversized).unwrap_err(),
        CredentialError::CredentialTooLarge
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[test]
fn encoded_invalid_field_name_is_corrupt_storage() {
    let backend = MemoryBackend::default();
    let key = CredentialKey::new("claude", "invalid-encoded-field").unwrap();
    let mut encoded = b"ULLAGEC2".to_vec();
    encoded.extend_from_slice(&1_u64.to_be_bytes());
    encoded.extend_from_slice(&1_u64.to_be_bytes());
    encoded.push(1);
    encoded.extend_from_slice(&1_u32.to_be_bytes());
    encoded.extend_from_slice(&12_u16.to_be_bytes());
    encoded.extend_from_slice(b"access/token");
    encoded.extend_from_slice(&6_u32.to_be_bytes());
    encoded.extend_from_slice(b"secret");
    backend.write(&key, &encoded).unwrap();
    let store = CredentialStore::new(backend);
    assert_eq!(
        store.get(&key).unwrap_err(),
        CredentialError::CorruptCredential
    );
}

#[test]
fn native_store_never_reports_file_fallback() {
    let kind = CredentialStore::new(NativeStore::new()).backend_kind();
    assert_ne!(kind, BackendKind::ExplicitFileFallback);
}

#[cfg(unix)]
#[test]
fn file_fallback_rejects_symlinked_directory() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let real = temporary.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = temporary.path().join("link");
    symlink(real, &link).unwrap();
    let error = FileStore::new(FileFallbackOptions::new(link).unwrap()).unwrap_err();
    assert_eq!(error, CredentialError::UnsafeFallbackPath);
}

#[cfg(unix)]
#[test]
fn file_fallback_rejects_leaf_symlink_creation_race() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("raced-vault");
    let target = temporary.path().join("unrelated");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let entered = Arc::new(ThreadBarrier::new(2));
    let release = Arc::new(ThreadBarrier::new(2));
    crate::file_store::set_directory_create_hook(Some((
        "raced-vault".into(),
        Arc::clone(&entered),
        Arc::clone(&release),
    )));

    let constructor =
        std::thread::spawn(move || FileStore::new(FileFallbackOptions::new(root).unwrap()));
    entered.wait();
    symlink(&target, temporary.path().join("raced-vault")).unwrap();
    release.wait();
    let error = constructor.join().unwrap().unwrap_err();
    crate::file_store::set_directory_create_hook(None);

    assert_eq!(error, CredentialError::UnsafeFallbackPath);
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[cfg(unix)]
#[test]
fn file_fallback_stays_on_open_directory_after_path_switch() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("vault");
    let moved = temporary.path().join("moved-vault");
    let attacker = temporary.path().join("attacker");
    let store =
        CredentialStore::new(FileStore::new(FileFallbackOptions::new(&root).unwrap()).unwrap());
    std::fs::create_dir(&attacker).unwrap();
    std::fs::rename(&root, &moved).unwrap();
    symlink(&attacker, &root).unwrap();

    let key = CredentialKey::new("claude", "fixed-handle").unwrap();
    store.set(&key, credential(b"anchored")).unwrap();
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&attacker).unwrap().count(), 0);
}
