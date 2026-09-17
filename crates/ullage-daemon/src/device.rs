use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ullage_protocol::{DevicePayload, PairCodePayload};

const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
const PAIR_CODE_LENGTH: usize = 6;
const DEVICE_ID_LENGTH: usize = 12;
const TOKEN_BYTES: usize = 32;
const PAIR_CODE_TTL_SECONDS: i64 = 300;
const PAIR_CODE_MAX_FAILURES: u8 = 5;
const LAST_SEEN_WRITE_INTERVAL_SECONDS: i64 = 60;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct DeviceStore {
    inner: Arc<Mutex<DeviceState>>,
}

struct DeviceState {
    path: Option<PathBuf>,
    devices: Vec<DeviceRecord>,
    pair_code: Option<PendingPairCode>,
    last_persisted_seen: HashMap<String, DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceFile {
    devices: Vec<DeviceRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeviceRecord {
    id: String,
    name: String,
    token_hash: String,
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<DateTime<Utc>>,
}

struct PendingPairCode {
    normalized: String,
    expires_at: DateTime<Utc>,
    failures: u8,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCredential {
    pub device_id: String,
    pub device_token: String,
    pub device_name: String,
}

// `device_token` is a bearer credential: never let it reach a log through
// `Debug`.
impl std::fmt::Debug for DeviceCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceCredential")
            .field("device_id", &self.device_id)
            .field("device_token", &"<redacted>")
            .field("device_name", &self.device_name)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairDeviceError {
    InvalidCode,
    InvalidName,
    Storage(String),
}

impl Default for DeviceStore {
    fn default() -> Self {
        Self::memory()
    }
}

impl DeviceStore {
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DeviceState {
                path: None,
                devices: Vec::new(),
                pair_code: None,
                last_persisted_seen: HashMap::new(),
            })),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        sweep_stale_temporary_files(&path);
        let devices = match read_private_file(&path)? {
            Some(bytes) => {
                let devices = serde_json::from_slice::<DeviceFile>(&bytes)
                    .map_err(|error| format!("{} could not be parsed: {error}", path.display()))?
                    .devices;
                validate_devices(&devices)
                    .map_err(|error| format!("{} is invalid: {error}", path.display()))?;
                devices
            }
            None => Vec::new(),
        };
        let last_persisted_seen = devices
            .iter()
            .map(|device| (device.id.clone(), device.last_seen_at))
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(DeviceState {
                path: Some(path),
                devices,
                pair_code: None,
                last_persisted_seen,
            })),
        })
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.lock().path.clone()
    }

    pub fn create_pair_code(&self) -> Result<PairCodePayload, String> {
        self.create_pair_code_at(Utc::now())
    }

    pub fn list_devices(&self) -> Vec<DevicePayload> {
        self.lock()
            .devices
            .iter()
            .filter(|device| device.revoked_at.is_none())
            .map(device_payload)
            .collect()
    }

    pub fn revoke_device(&self, device_id: &str) -> Result<bool, String> {
        self.revoke_device_at(device_id, Utc::now())
    }

    pub fn pair(
        &self,
        pair_code: &str,
        device_name: &str,
    ) -> Result<DeviceCredential, PairDeviceError> {
        self.pair_at(pair_code, device_name, Utc::now())
    }

    pub fn authenticate(&self, token: &str) -> Result<bool, String> {
        self.authenticate_at(token, Utc::now())
    }

    fn create_pair_code_at(&self, now: DateTime<Utc>) -> Result<PairCodePayload, String> {
        self.create_pair_code_at_with(now, || random_alphabet_string(PAIR_CODE_LENGTH))
    }

    fn create_pair_code_at_with(
        &self,
        now: DateTime<Utc>,
        mut generate: impl FnMut() -> Result<String, String>,
    ) -> Result<PairCodePayload, String> {
        let mut state = self.lock();
        let normalized = loop {
            let candidate = generate()?;
            if state
                .pair_code
                .as_ref()
                .is_none_or(|pending| pending.normalized != candidate)
            {
                break candidate;
            }
        };
        // The display split below assumes exactly PAIR_CODE_LENGTH characters;
        // a custom generator must uphold that or the code cannot be entered.
        if normalized.chars().count() != PAIR_CODE_LENGTH {
            return Err("pair code generator returned a malformed code".into());
        }
        let expires_at = now + TimeDelta::seconds(PAIR_CODE_TTL_SECONDS);
        state.pair_code = Some(PendingPairCode {
            normalized: normalized.clone(),
            expires_at,
            failures: 0,
        });
        Ok(PairCodePayload {
            code: format!("{}-{}", &normalized[..3], &normalized[3..]),
            expires_at,
        })
    }

    fn revoke_device_at(&self, device_id: &str, now: DateTime<Utc>) -> Result<bool, String> {
        let mut state = self.lock();
        let Some(index) = state
            .devices
            .iter()
            .position(|device| device.id == device_id)
        else {
            return Ok(false);
        };
        if state.devices[index].revoked_at.is_some() {
            return Ok(true);
        }
        state.devices[index].revoked_at = Some(now);
        if let Err(error) = persist_state(&state) {
            state.devices[index].revoked_at = None;
            return Err(error);
        }
        Ok(true)
    }

    fn pair_at(
        &self,
        pair_code: &str,
        device_name: &str,
        now: DateTime<Utc>,
    ) -> Result<DeviceCredential, PairDeviceError> {
        // Length is validated on the sanitized name so the limit matches what
        // is actually stored and shown; `validate_devices` re-checks on load.
        let name = sanitize_device_name(device_name);
        if name.chars().count() > 64 {
            return Err(PairDeviceError::InvalidName);
        }
        let normalized = normalize_pair_code(pair_code);
        let mut state = self.lock();
        let valid = if let Some(pending) = state.pair_code.as_ref() {
            now < pending.expires_at
                && constant_time_eq(
                    normalized.as_deref().unwrap_or_default().as_bytes(),
                    pending.normalized.as_bytes(),
                )
        } else {
            false
        };
        if !valid {
            if let Some(pending) = state.pair_code.as_mut() {
                if now >= pending.expires_at {
                    state.pair_code = None;
                } else {
                    pending.failures = pending.failures.saturating_add(1);
                    if pending.failures >= PAIR_CODE_MAX_FAILURES {
                        state.pair_code = None;
                    }
                }
            }
            return Err(PairDeviceError::InvalidCode);
        }

        let device_id = loop {
            let candidate =
                random_alphabet_string(DEVICE_ID_LENGTH).map_err(PairDeviceError::Storage)?;
            if state.devices.iter().all(|device| device.id != candidate) {
                break candidate;
            }
        };
        let device_token = generate_device_token().map_err(PairDeviceError::Storage)?;
        let record = DeviceRecord {
            id: device_id.clone(),
            name: name.clone(),
            token_hash: token_hash(&device_token),
            created_at: now,
            last_seen_at: now,
            revoked_at: None,
        };
        state.devices.push(record);
        if let Err(error) = persist_state(&state) {
            state.devices.pop();
            return Err(PairDeviceError::Storage(error));
        }
        state.last_persisted_seen.insert(device_id.clone(), now);
        state.pair_code = None;
        Ok(DeviceCredential {
            device_id,
            device_token,
            device_name: name,
        })
    }

    fn authenticate_at(&self, token: &str, now: DateTime<Utc>) -> Result<bool, String> {
        let presented_hash = token_hash(token);
        let mut state = self.lock();
        let mut matches = Vec::new();
        for (index, device) in state.devices.iter().enumerate() {
            let eligible = device.revoked_at.is_none();
            let equal = constant_time_eq(presented_hash.as_bytes(), device.token_hash.as_bytes());
            if eligible & equal {
                matches.push(index);
            }
        }
        if matches.is_empty() {
            return Ok(false);
        }

        for index in &matches {
            let device = &mut state.devices[*index];
            device.last_seen_at = device.last_seen_at.max(device.created_at).max(now);
        }
        let persisted = matches
            .iter()
            .filter_map(|index| {
                let device = &state.devices[*index];
                let is_due = state
                    .last_persisted_seen
                    .get(&device.id)
                    .is_none_or(|last| {
                        device
                            .last_seen_at
                            .signed_duration_since(*last)
                            .num_seconds()
                            >= LAST_SEEN_WRITE_INTERVAL_SECONDS
                    });
                is_due.then(|| (device.id.clone(), device.last_seen_at))
            })
            .collect::<Vec<_>>();
        if !persisted.is_empty() {
            let persisted_ids = persisted
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            persist_state_with_current_seen(&state, &persisted_ids)?;
            for (id, last_seen_at) in persisted {
                state.last_persisted_seen.insert(id, last_seen_at);
            }
        }
        Ok(true)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DeviceState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn device_payload(device: &DeviceRecord) -> DevicePayload {
    DevicePayload {
        id: device.id.clone(),
        name: device.name.clone(),
        created_at: device.created_at,
        last_seen_at: device.last_seen_at,
    }
}

fn validate_devices(devices: &[DeviceRecord]) -> Result<(), &'static str> {
    let mut ids = HashSet::with_capacity(devices.len());
    for device in devices {
        if device.id.len() != DEVICE_ID_LENGTH
            || !device.id.bytes().all(|byte| ALPHABET.contains(&byte))
        {
            return Err("device id is malformed");
        }
        if !ids.insert(&device.id) {
            return Err("device id is duplicated");
        }
        if device.name.is_empty()
            || device.name.chars().count() > 64
            || !device.name.chars().all(device_name_character_is_safe)
        {
            return Err("device name is malformed");
        }
        if device.last_seen_at < device.created_at
            || device
                .revoked_at
                .is_some_and(|revoked_at| revoked_at < device.created_at)
        {
            return Err("device timestamps are malformed");
        }
        if device.token_hash.len() != 64
            || !device
                .token_hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("device token hash is malformed");
        }
    }
    Ok(())
}

fn sanitize_device_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .filter(|character| device_name_character_is_safe(*character))
        .collect::<String>();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

fn device_name_character_is_safe(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn normalize_pair_code(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let compact = match bytes {
        [a, b, c, b'-', d, e, f] => [*a, *b, *c, *d, *e, *f],
        [a, b, c, d, e, f] => [*a, *b, *c, *d, *e, *f],
        _ => return None,
    };
    let mut normalized = String::with_capacity(PAIR_CODE_LENGTH);
    for byte in compact {
        let upper = byte.to_ascii_uppercase();
        if !ALPHABET.contains(&upper) {
            return None;
        }
        normalized.push(char::from(upper));
    }
    Some(normalized)
}

fn random_alphabet_string(length: usize) -> Result<String, String> {
    random_alphabet_string_with(length, |bytes| {
        getrandom::fill(bytes).map_err(|_| "secure random generation failed".to_owned())
    })
}

fn random_alphabet_string_with(
    length: usize,
    mut fill: impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<String, String> {
    let alphabet_len = u16::try_from(ALPHABET.len()).unwrap();
    let cutoff = 256 - (256 % alphabet_len);
    let mut value = String::with_capacity(length);
    let mut random = [0_u8; 32];
    while value.len() < length {
        fill(&mut random)?;
        for byte in random {
            if u16::from(byte) >= cutoff {
                continue;
            }
            value.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            if value.len() == length {
                break;
            }
        }
    }
    Ok(value)
}

pub fn generate_device_token() -> Result<String, String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| "device token could not be generated".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(left_byte ^ right_byte);
    }
    diff == 0
}

/// Device mutations serialize on the store mutex and hold it across the
/// atomic write. The file is a small local JSON document (one fsync plus one
/// rename), so the critical section stays bounded; callers are one-shot
/// control commands, not a hot path.
fn persist_state(state: &DeviceState) -> Result<(), String> {
    persist_state_with_current_seen(state, &[])
}

fn persist_state_with_current_seen(
    state: &DeviceState,
    current_seen_ids: &[String],
) -> Result<(), String> {
    let Some(path) = state.path.as_deref() else {
        return Ok(());
    };
    let devices = state
        .devices
        .iter()
        .map(|device| {
            let mut persisted = device.clone();
            if !current_seen_ids.contains(&device.id) {
                if let Some(last_seen_at) = state.last_persisted_seen.get(&device.id) {
                    persisted.last_seen_at = *last_seen_at;
                }
            }
            persisted
        })
        .collect::<Vec<_>>();
    write_device_file(path, &devices)
}

fn write_device_file(path: &Path, devices: &[DeviceRecord]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&DeviceFile {
        devices: devices.to_vec(),
    })
    .map_err(|error| format!("{} could not be serialized: {error}", path.display()))?;
    replace_private_file(path, &bytes)
}

fn replace_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_private_parent(path)?;
    validate_existing_private_file(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no valid file name", path.display()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ));
    let _guard = TemporaryFile(temporary.clone());
    create_private_file(&temporary, bytes)?;
    replace_path(&temporary, path)
        .map_err(|error| format!("{} could not be replaced: {error}", path.display()))?;
    // The rename is only durable once the directory entry is on disk.
    sync_parent_directory(path)
        .map_err(|error| format!("{} could not be synced: {error}", path.display()))
}

/// Removes temp files left behind by a crashed `replace_private_file`, matched
/// on the `.name.pid.sequence.tmp` naming this store generates. Best effort: a
/// file that cannot be removed is left for the next open.
fn sweep_stale_temporary_files(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{file_name}.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_stale_temporary_name(&name, &prefix) {
            continue;
        }
        if entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Matches only the `.name.digits.digits.tmp` temp names `replace_private_file`
/// generates, so the sweep never touches a user file that merely looks similar.
fn is_stale_temporary_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some(body) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let mut parts = body.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    numeric(parts.next()) && numeric(parts.next()) && parts.next().is_none()
}

/// fsyncs the directory holding `path` so a committed rename survives a crash.
/// On Windows the write-through rename already flushes the directory entry.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let path = std::ffi::CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| format!("{} has no valid parent path", path.display()))?;
    // SAFETY: `path` is a valid null-terminated path; the fd is checked below
    // and ownership is passed to `File` only on success.
    let descriptor = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: `descriptor` is a valid, freshly opened fd owned by this scope.
    let directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    directory.sync_all().map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn read_private_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    ensure_private_parent(path)?;
    let Some(mut file) = open_private_file(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn validate_existing_private_file(path: &Path) -> Result<(), String> {
    open_private_file(path).map(|_| ())
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}

fn ensure_private_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

        let mut missing = Vec::new();
        let mut existing = parent;
        let existing_metadata = loop {
            match std::fs::symlink_metadata(existing) {
                Ok(metadata) => break metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    missing.push(existing.to_path_buf());
                    existing = existing
                        .parent()
                        .ok_or_else(|| format!("{} has no existing ancestor", path.display()))?;
                }
                Err(error) => {
                    return Err(format!(
                        "{} could not be inspected: {error}",
                        parent.display()
                    ));
                }
            }
        };
        if existing_metadata.file_type().is_symlink() || !existing_metadata.is_dir() {
            return Err(format!(
                "{} ancestor must be a real directory",
                parent.display()
            ));
        }
        for directory in missing.into_iter().rev() {
            match std::fs::symlink_metadata(&directory) {
                Ok(_) => return Err(format!("{} changed during creation", directory.display())),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "{} could not be inspected: {error}",
                        directory.display()
                    ));
                }
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&directory).map_err(|error| {
                format!("{} could not be created: {error}", directory.display())
            })?;
            let metadata = std::fs::symlink_metadata(&directory).map_err(|error| {
                format!("{} could not be inspected: {error}", directory.display())
            })?;
            // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(format!(
                    "{} is not a private directory",
                    directory.display()
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(parent)
            .map_err(|error| format!("{} could not be inspected: {error}", parent.display()))?;
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!("{} must be a private directory", parent.display()));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        ensure_private_windows_directory(parent)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{} could not be created: {error}", parent.display()))
    }
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options
            .open(path)
            .map_err(|error| format!("{} could not be created: {error}", path.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{} could not be written: {error}", path.display()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        let mut file = ullage_auth::create_private_windows_file(path)
            .map_err(|error| format!("{} could not be created: {error}", path.display()))?;
        if !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
            .map_err(|error| format!("{} ACL validation failed: {error}", path.display()))?
        {
            return Err(format!("{} must be private", path.display()));
        }
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{} could not be written: {error}", path.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, bytes);
        Err("device storage is unsupported on this platform".into())
    }
}

fn open_private_file(path: &Path) -> Result<Option<std::fs::File>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(format!("{} must be a regular file", path.display()));
            }
            Err(error) => return Err(format!("{} could not be opened: {error}", path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("{} could not be validated: {error}", path.display()))?;
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(format!("{} must be a private regular file", path.display()));
        }
        Ok(Some(file))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{} could not be opened: {error}", path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("{} could not be validated: {error}", path.display()))?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle())
                .map_err(|error| format!("{} ACL validation failed: {error}", path.display()))?
        {
            return Err(format!("{} must be a private regular file", path.display()));
        }
        Ok(Some(file))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err("device storage is unsupported on this platform".into())
    }
}

#[cfg(unix)]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are valid, null-terminated UTF-16 buffers for the duration of the call.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_path(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn ensure_private_windows_directory(path: &Path) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                missing.push(existing.to_path_buf());
                existing = existing.parent().ok_or_else(|| {
                    format!("{} has no existing Windows ancestor", path.display())
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "{} could not be inspected: {error}",
                    path.display()
                ));
            }
        }
    }
    for directory in missing.into_iter().rev() {
        ullage_auth::create_private_windows_directory(&directory)
            .map_err(|error| format!("{} could not be created: {error}", directory.display()))?;
    }
    validate_private_windows_directory(path)
}

#[cfg(windows)]
fn validate_private_windows_directory(path: &Path) -> Result<(), String> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(GENERIC_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options
        .open(path)
        .map_err(|error| format!("{} could not be opened: {error}", path.display()))?;
    let metadata = directory
        .metadata()
        .map_err(|error| format!("{} could not be validated: {error}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !ullage_auth::windows_handle_acl_is_private(directory.as_raw_handle())
            .map_err(|error| format!("{} ACL validation failed: {error}", path.display()))?
    {
        return Err(format!(
            "{} must be a private non-reparse directory",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn generated_codes_use_the_alphabet_and_external_format() {
        let store = DeviceStore::memory();
        for _ in 0..1000 {
            let code = store.create_pair_code().unwrap().code;
            assert_eq!(code.len(), 7);
            assert_eq!(&code[3..4], "-");
            assert!(
                code.bytes()
                    .filter(|byte| *byte != b'-')
                    .all(|byte| ALPHABET.contains(&byte))
            );
        }
    }

    #[test]
    fn random_generation_rejects_out_of_range_bytes() {
        let mut calls = 0;
        let value = random_alphabet_string_with(6, |bytes| {
            calls += 1;
            bytes.fill(if calls == 1 { u8::MAX } else { 0 });
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(value, "222222");
    }

    #[test]
    fn pair_code_parser_accepts_only_the_contract_shapes() {
        assert_eq!(normalize_pair_code("ABC-DEF").as_deref(), Some("ABCDEF"));
        assert_eq!(normalize_pair_code("abcdef").as_deref(), Some("ABCDEF"));
        for invalid in [
            "ABCDE", "ABCDEFG", "AB-CDEF", "ABC--DEF", "ABC DEF", "ABC\tDEF", "0BCDEF", "OBCDEF",
            "IBCDEF", "LBCDEF", "UBCDEF", "1BCDEF",
        ] {
            assert_eq!(normalize_pair_code(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn device_names_drop_unsafe_controls_and_fall_back_when_empty() {
        assert_eq!(sanitize_device_name("pro\0\n2026"), "pro2026");
        assert_eq!(sanitize_device_name("lab\u{202e}host"), "labhost");
        assert_eq!(sanitize_device_name("\0\n\t\u{202e}"), "unknown");
    }

    #[test]
    fn unsafe_device_names_never_reach_list_payloads_or_persisted_validation() {
        let store = DeviceStore::memory();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let credential = store
            .pair_at(&code.code, "lab\u{202e}host", fixed_time(0))
            .unwrap();
        assert_eq!(credential.device_name, "labhost");
        assert_eq!(store.list_devices()[0].name, "labhost");

        let invalid = DeviceRecord {
            id: "222222222222".to_owned(),
            name: "lab\u{202e}host".to_owned(),
            token_hash: "00".repeat(32),
            created_at: fixed_time(0),
            last_seen_at: fixed_time(0),
            revoked_at: None,
        };
        assert_eq!(
            validate_devices(&[invalid]),
            Err("device name is malformed")
        );
    }

    #[test]
    fn pair_codes_expire_are_single_use_and_are_replaced() {
        let store = DeviceStore::memory();
        let first = store.create_pair_code_at(fixed_time(0)).unwrap();
        let replacement = store.create_pair_code_at(fixed_time(1)).unwrap();
        assert_eq!(
            store.pair_at(&first.code, "first", fixed_time(2)),
            Err(PairDeviceError::InvalidCode)
        );
        let credential = store
            .pair_at(&replacement.code, "device", fixed_time(2))
            .unwrap();
        assert!(
            store
                .authenticate_at(&credential.device_token, fixed_time(2))
                .unwrap()
        );
        assert_eq!(
            store.pair_at(&replacement.code, "again", fixed_time(2)),
            Err(PairDeviceError::InvalidCode)
        );

        let expired = store.create_pair_code_at(fixed_time(10)).unwrap();
        assert_eq!(
            store.pair_at(&expired.code, "late", fixed_time(310)),
            Err(PairDeviceError::InvalidCode)
        );
    }

    #[test]
    fn replacement_pair_code_retries_when_generation_repeats() {
        let store = DeviceStore::memory();
        let first = store
            .create_pair_code_at_with(fixed_time(0), || Ok("222222".to_owned()))
            .unwrap();
        let mut candidates = ["222222", "333333"].into_iter();
        let replacement = store
            .create_pair_code_at_with(fixed_time(1), || Ok(candidates.next().unwrap().to_owned()))
            .unwrap();

        assert_eq!(first.code, "222-222");
        assert_eq!(replacement.code, "333-333");
        assert_eq!(
            store.pair_at(&first.code, "old", fixed_time(2)),
            Err(PairDeviceError::InvalidCode)
        );
        assert!(
            store
                .pair_at(&replacement.code, "new", fixed_time(2))
                .is_ok()
        );
    }

    #[test]
    fn constant_time_comparison_rejects_length_and_value_differences() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcde"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcdef"));
    }

    #[test]
    fn five_failures_invalidate_the_current_pair_code() {
        let store = DeviceStore::memory();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let mut wrong = code.code.clone().into_bytes();
        wrong[0] = if wrong[0] == b'2' { b'3' } else { b'2' };
        let wrong = String::from_utf8(wrong).unwrap();
        for _ in 0..5 {
            assert_eq!(
                store.pair_at(&wrong, "wrong", fixed_time(1)),
                Err(PairDeviceError::InvalidCode)
            );
        }
        assert_eq!(
            store.pair_at(&code.code, "too late", fixed_time(1)),
            Err(PairDeviceError::InvalidCode)
        );
    }

    #[cfg(unix)]
    #[test]
    fn persistent_store_is_private_hashed_and_reloadable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        let store = DeviceStore::open(&path).unwrap();
        let code = store.create_pair_code().unwrap();
        let credential = store.pair(&code.code, "device").unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(credential.device_token.len())
                .any(|window| window == credential.device_token.as_bytes())
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let restored = DeviceStore::open(&path).unwrap();
        assert!(restored.authenticate(&credential.device_token).unwrap());
        assert!(restored.revoke_device(&credential.device_id).unwrap());
        assert!(restored.revoke_device(&credential.device_id).unwrap());
        let reloaded = DeviceStore::open(&path).unwrap();
        assert!(!reloaded.authenticate(&credential.device_token).unwrap());
        assert!(reloaded.list_devices().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn opening_a_missing_store_does_not_replace_a_winner() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        let losing_startup = DeviceStore::open(&path).unwrap();
        assert!(!path.exists());

        let winner = DeviceStore::open(&path).unwrap();
        let code = winner.create_pair_code_at(fixed_time(0)).unwrap();
        let credential = winner.pair_at(&code.code, "winner", fixed_time(0)).unwrap();
        let persisted = std::fs::read(&path).unwrap();

        drop(losing_startup);
        assert_eq!(std::fs::read(&path).unwrap(), persisted);
        assert!(
            DeviceStore::open(&path)
                .unwrap()
                .authenticate_at(&credential.device_token, fixed_time(1))
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn last_seen_writes_are_throttled_for_each_device() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        let store = DeviceStore::open(&path).unwrap();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let credential = store.pair_at(&code.code, "device", fixed_time(0)).unwrap();
        let initial = std::fs::read(&path).unwrap();
        assert!(
            store
                .authenticate_at(&credential.device_token, fixed_time(59))
                .unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), initial);
        assert!(
            store
                .authenticate_at(&credential.device_token, fixed_time(60))
                .unwrap()
        );
        assert_ne!(std::fs::read(&path).unwrap(), initial);
    }

    #[test]
    fn last_seen_remains_monotonic_when_the_clock_moves_backward() {
        let store = DeviceStore::memory();
        let code = store.create_pair_code_at(fixed_time(100)).unwrap();
        let credential = store
            .pair_at(&code.code, "device", fixed_time(100))
            .unwrap();

        assert!(
            store
                .authenticate_at(&credential.device_token, fixed_time(50))
                .unwrap()
        );
        let listed = store.list_devices();
        assert_eq!(listed[0].created_at, fixed_time(100));
        assert_eq!(listed[0].last_seen_at, fixed_time(100));
    }

    #[cfg(unix)]
    #[test]
    fn full_file_writes_preserve_non_due_last_seen_values() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        let store = DeviceStore::open(&path).unwrap();

        let first_code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let first = store
            .pair_at(&first_code.code, "first", fixed_time(0))
            .unwrap();
        let second_code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let second = store
            .pair_at(&second_code.code, "second", fixed_time(0))
            .unwrap();

        assert!(
            store
                .authenticate_at(&second.device_token, fixed_time(30))
                .unwrap()
        );
        assert!(
            store
                .authenticate_at(&first.device_token, fixed_time(60))
                .unwrap()
        );
        let persisted: DeviceFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let first_record = persisted
            .devices
            .iter()
            .find(|device| device.id == first.device_id)
            .unwrap();
        let second_record = persisted
            .devices
            .iter()
            .find(|device| device.id == second.device_id)
            .unwrap();
        assert_eq!(first_record.last_seen_at, fixed_time(60));
        assert_eq!(second_record.last_seen_at, fixed_time(0));

        assert!(
            store
                .authenticate_at(&second.device_token, fixed_time(60))
                .unwrap()
        );
        let after_second_write = std::fs::read(&path).unwrap();
        assert!(
            store
                .authenticate_at(&second.device_token, fixed_time(119))
                .unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), after_second_write);
    }

    #[test]
    fn a_pair_code_is_dead_at_its_expiry_instant() {
        let store = DeviceStore::memory();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        assert_eq!(
            store.pair_at(&code.code, "edge", fixed_time(300)),
            Err(PairDeviceError::InvalidCode)
        );
        assert_eq!(
            store.pair_at(&code.code, "edge", fixed_time(299)),
            Err(PairDeviceError::InvalidCode)
        );
    }

    #[test]
    fn unparseable_pair_codes_count_toward_the_failure_limit() {
        let store = DeviceStore::memory();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        for _ in 0..5 {
            assert_eq!(
                store.pair_at("not-a-code", "wrong", fixed_time(1)),
                Err(PairDeviceError::InvalidCode)
            );
        }
        assert_eq!(
            store.pair_at(&code.code, "too late", fixed_time(1)),
            Err(PairDeviceError::InvalidCode)
        );
    }

    #[test]
    fn device_credential_debug_never_exposes_the_token() {
        let credential = DeviceCredential {
            device_id: "device-1".into(),
            device_token: "s3cr3t-token".into(),
            device_name: "laptop".into(),
        };
        let rendered = format!("{credential:?}");
        assert!(rendered.contains("device-1"));
        assert!(!rendered.contains("s3cr3t-token"));
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_a_corrupt_device_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        std::fs::write(&path, b"not json").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            DeviceStore::open(&path)
                .err()
                .is_some_and(|error| error.contains("could not be parsed"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn authenticate_returns_an_error_when_the_seen_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("devices.json");
        let store = DeviceStore::open(&path).unwrap();
        let code = store.create_pair_code_at(fixed_time(0)).unwrap();
        let credential = store.pair_at(&code.code, "device", fixed_time(0)).unwrap();

        // Replacing the parent with a file makes the next atomic write fail
        // while the in-memory state stays consistent.
        std::fs::remove_dir_all(directory.path()).unwrap();
        std::fs::write(directory.path(), b"blocked").unwrap();
        assert!(
            store
                .authenticate_at(&credential.device_token, fixed_time(120))
                .is_err()
        );
    }
}
