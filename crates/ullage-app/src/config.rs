use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs::File, io::Read};

use serde::{Deserialize, Serialize};
use ullage_auth::CredentialKey;
use ullage_core::{ProviderId, UsageQuery, summary::MetricFilter};
use ullage_daemon::{AccountConfig, AccountId, BackoffConfig, DaemonConfig, ProviderLimit};
use ullage_http::parse_http_bind;

pub const CONFIG_VERSION: u16 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const PROVIDERS: [&str; 4] = ["claude", "chatgpt", "grok", "cursor"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub version: u16,
    #[serde(default)]
    pub daemon: DaemonSettings,
    #[serde(default)]
    pub providers: ProviderSettings,
    #[serde(default)]
    pub credentials: CredentialsSettings,
    #[serde(default)]
    pub http: HttpSettings,
    #[serde(default)]
    pub accounts: Vec<AccountSettings>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            daemon: DaemonSettings::default(),
            providers: ProviderSettings::default(),
            credentials: CredentialsSettings::default(),
            http: HttpSettings::default(),
            accounts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialsSettings {
    pub file_fallback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpSettings {
    pub enabled: bool,
    pub bind: String,
    pub allowed_origins: Vec<String>,
    pub probe_min_interval_seconds: u64,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:7878".into(),
            allowed_origins: Vec::new(),
            probe_min_interval_seconds: 60,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonSettings {
    pub maximum_concurrency: usize,
    pub default_provider_concurrency: usize,
    pub provider_limits: Vec<ProviderLimitSettings>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            maximum_concurrency: 8,
            default_provider_concurrency: 2,
            provider_limits: Vec::new(),
        }
    }
}

impl DaemonSettings {
    pub fn build(&self) -> DaemonConfig {
        DaemonConfig {
            maximum_concurrency: self.maximum_concurrency,
            default_provider_concurrency: self.default_provider_concurrency,
            provider_limits: self
                .provider_limits
                .iter()
                .map(|limit| ProviderLimit {
                    provider: ProviderId::new(&limit.provider),
                    maximum_concurrency: limit.maximum_concurrency,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLimitSettings {
    pub provider: String,
    pub maximum_concurrency: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountSettings {
    pub id: String,
    pub provider: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub jitter_seconds: u64,
    pub backoff_initial_seconds: u64,
    pub backoff_maximum_seconds: u64,
    /// Display metric names seeded into a newly created account; empty means no
    /// filter.
    pub metrics: Vec<String>,
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider: String::new(),
            label: None,
            enabled: true,
            interval_seconds: 300,
            timeout_seconds: 30,
            jitter_seconds: 5,
            backoff_initial_seconds: 30,
            backoff_maximum_seconds: 1800,
            metrics: Vec::new(),
        }
    }
}

impl AccountSettings {
    pub fn build(&self) -> Result<AccountConfig, String> {
        let metrics = MetricFilter::new(self.metrics.clone()).map_err(|error| {
            format!(
                "configured account metrics are invalid for account {}: {error}",
                self.id
            )
        })?;
        Ok(AccountConfig {
            id: AccountId::new(&self.id),
            provider: ProviderId::new(&self.provider),
            query: UsageQuery {
                account_label: self.label.clone(),
            },
            enabled: self.enabled,
            interval: Duration::from_secs(self.interval_seconds),
            timeout: Duration::from_secs(self.timeout_seconds),
            jitter: Duration::from_secs(self.jitter_seconds),
            backoff: BackoffConfig {
                initial: Duration::from_secs(self.backoff_initial_seconds),
                maximum: Duration::from_secs(self.backoff_maximum_seconds),
            },
            metrics: metrics.names().to_vec(),
        })
    }
}

pub async fn load(path: &Path) -> Result<AppConfig, String> {
    let bytes = match read_config(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppConfig::default());
        }
        Err(error) => return Err(error.to_string()),
    };
    let config: AppConfig = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if config.version != CONFIG_VERSION {
        return Err(format!(
            "unsupported configuration version {}; expected {CONFIG_VERSION}",
            config.version
        ));
    }
    validate(&config)?;
    Ok(config)
}

fn read_config(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path)?;
    validate_config_handle(&file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "configuration file exceeds the supported size",
        ));
    }
    Ok(bytes)
}

fn validate_config_handle(file: &File) -> Result<(), std::io::Error> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "configuration path must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: `geteuid` has no arguments and no memory-safety preconditions.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "configuration file must be owned by and accessible only to the current user",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::io::AsRawHandle;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !ullage_auth::windows_handle_acl_is_private(file.as_raw_handle()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "configuration ACL validation failed",
                )
            })?
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "configuration file must be owned by and accessible only to the current user",
            ));
        }
    }
    Ok(())
}

fn validate(config: &AppConfig) -> Result<(), String> {
    if config.daemon.maximum_concurrency == 0 || config.daemon.default_provider_concurrency == 0 {
        return Err("daemon concurrency must be greater than zero".into());
    }
    if config.http.probe_min_interval_seconds == 0 {
        return Err("http.probe_min_interval_seconds must be greater than zero".into());
    }
    if config
        .http
        .allowed_origins
        .iter()
        .any(|origin| origin == "*")
    {
        return Err("http.allowed_origins must not contain *".into());
    }
    if config.http.enabled {
        parse_http_bind(&config.http.bind)?;
    }
    let mut limited_providers = std::collections::BTreeSet::new();
    for limit in &config.daemon.provider_limits {
        if !PROVIDERS.contains(&limit.provider.as_str())
            || limit.maximum_concurrency == 0
            || !limited_providers.insert(&limit.provider)
        {
            return Err("provider concurrency limits must be unique and valid".into());
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut selectors = std::collections::BTreeSet::new();
    for account in &config.accounts {
        if account.id.trim().is_empty()
            || !PROVIDERS.contains(&account.provider.as_str())
            || CredentialKey::new(&account.provider, &account.id).is_err()
            || account
                .id
                .chars()
                .any(ullage_core::is_unsafe_identity_character)
            || account.label.as_deref().is_some_and(|label| {
                label.trim().is_empty()
                    || label.chars().any(ullage_core::is_unsafe_identity_character)
            })
        {
            return Err("configured account identity is invalid".into());
        }
        if let Err(error) = MetricFilter::new(account.metrics.clone()) {
            return Err(format!(
                "configured account metrics are invalid for account {}: {error}",
                account.id
            ));
        }
        if !ids.insert(&account.id) {
            return Err("configured account ids must be unique".into());
        }
        if !selectors.insert((&account.provider, &account.label)) {
            return Err("configured provider account selectors must be unique".into());
        }
        if account.interval_seconds == 0
            || account.timeout_seconds == 0
            || account.backoff_initial_seconds == 0
            || account.backoff_maximum_seconds == 0
            || account.backoff_initial_seconds > account.backoff_maximum_seconds
        {
            return Err("configured account timing is invalid".into());
        }
    }
    Ok(())
}

pub fn default_config_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ULLAGE_CONFIG_FILE") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join("Library/Application Support/Ullage/config.json"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(root).join("ullage/config.json"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home).join(".config/ullage/config.json"));
        }
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("APPDATA") {
        return Ok(PathBuf::from(root).join("Ullage/config.json"));
    }
    Err("configuration path unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[tokio::test]
    async fn loads_versioned_accounts_without_accepting_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        tokio::fs::write(
            &path,
            br#"{
                "version": 1,
                "accounts": [{"id":"claude-a","provider":"claude","label":"a@example.test"}]
            }"#,
        )
        .await
        .unwrap();
        make_private(&path);
        let config = load(&path).await.unwrap();
        assert_eq!(config.accounts[0].id, "claude-a");
        assert_eq!(
            config.accounts[0]
                .build()
                .unwrap()
                .query
                .account_label
                .as_deref(),
            Some("a@example.test")
        );
        assert!(!config.credentials.file_fallback);
        assert!(!config.http.enabled);
        assert_eq!(config.http.bind, "127.0.0.1:7878");
        assert!(config.http.allowed_origins.is_empty());
        assert_eq!(config.http.probe_min_interval_seconds, 60);

        tokio::fs::write(&path, br#"{"version":1,"credentials":{"token":"secret"}}"#)
            .await
            .unwrap();
        make_private(&path);
        assert!(load(&path).await.is_err());
    }

    #[tokio::test]
    async fn loads_metric_filters_and_rejects_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        tokio::fs::write(
            &path,
            br#"{"version":1,"accounts":[{"id":"claude-a","provider":"claude","metrics":[" Usage ","Codex","usage"]}]}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        let config = load(&path).await.unwrap();
        assert_eq!(
            config.accounts[0].metrics,
            vec![" Usage ", "Codex", "usage"],
            "parsing keeps the raw spelling"
        );
        assert_eq!(
            config.accounts[0].build().unwrap().metrics,
            vec!["Usage", "Codex"],
            "building normalizes the filter"
        );

        // A configuration written before metrics existed stays valid.
        tokio::fs::write(
            &path,
            br#"{"version":1,"accounts":[{"id":"claude-a","provider":"claude"}]}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        assert!(load(&path).await.unwrap().accounts[0].metrics.is_empty());

        for metrics in [r#"[""]"#, r#"["safe","\u0007bad"]"#, r#"["\u202ebad"]"#] {
            let bytes = format!(
                r#"{{"version":1,"accounts":[{{"id":"claude-a","provider":"claude","metrics":{metrics}}}]}}"#
            );
            tokio::fs::write(&path, bytes).await.unwrap();
            make_private(&path);
            let error = load(&path).await.unwrap_err();
            assert!(error.contains("metrics"), "{metrics}: {error}");
        }
    }

    #[tokio::test]
    async fn loads_credentials_file_fallback_default_and_explicit_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        tokio::fs::write(&path, br#"{"version":1}"#).await.unwrap();
        make_private(&path);
        assert!(!load(&path).await.unwrap().credentials.file_fallback);

        tokio::fs::write(&path, br#"{"version":1,"credentials":{}}"#)
            .await
            .unwrap();
        make_private(&path);
        assert!(!load(&path).await.unwrap().credentials.file_fallback);

        tokio::fs::write(
            &path,
            br#"{"version":1,"credentials":{"file_fallback":true}}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        assert!(load(&path).await.unwrap().credentials.file_fallback);

        tokio::fs::write(
            &path,
            br#"{"version":1,"http":{"enabled":true,"bind":"127.0.0.1:9000","allowed_origins":["https://gui.example.test"],"probe_min_interval_seconds":15}}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        let http = load(&path).await.unwrap().http;
        assert!(http.enabled);
        assert_eq!(http.bind, "127.0.0.1:9000");
        assert_eq!(
            http.allowed_origins,
            vec!["https://gui.example.test".to_owned()]
        );
        assert_eq!(http.probe_min_interval_seconds, 15);

        tokio::fs::write(
            &path,
            br#"{"version":1,"http":{"probe_min_interval_seconds":0}}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        assert!(
            load(&path)
                .await
                .unwrap_err()
                .contains("http.probe_min_interval_seconds")
        );

        tokio::fs::write(&path, br#"{"version":1,"http":{"allowed_origins":["*"]}}"#)
            .await
            .unwrap();
        make_private(&path);
        assert!(
            load(&path)
                .await
                .unwrap_err()
                .contains("http.allowed_origins")
        );

        tokio::fs::write(
            &path,
            br#"{"version":1,"credentials":{"file_fallback":false,"unknown":true}}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        assert!(load(&path).await.is_err());
    }

    #[tokio::test]
    async fn validates_enabled_http_bind_addresses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for bind in [
            "127.0.0.1:7878",
            "100.64.0.1:7878",
            "192.168.50.10:7878",
            "auto:7878",
        ] {
            tokio::fs::write(
                &path,
                format!(r#"{{"version":1,"http":{{"enabled":true,"bind":"{bind}"}}}}"#),
            )
            .await
            .unwrap();
            make_private(&path);
            assert_eq!(load(&path).await.unwrap().http.bind, bind);
        }

        for bind in ["8.8.8.8:7878", "0.0.0.0:7878"] {
            tokio::fs::write(
                &path,
                format!(r#"{{"version":1,"http":{{"enabled":true,"bind":"{bind}"}}}}"#),
            )
            .await
            .unwrap();
            make_private(&path);
            let error = load(&path).await.unwrap_err();
            assert!(error.contains("http.bind"), "{bind}: {error}");
        }

        tokio::fs::write(
            &path,
            br#"{"version":1,"http":{"enabled":false,"bind":"not-a-bind"}}"#,
        )
        .await
        .unwrap();
        make_private(&path);
        assert_eq!(load(&path).await.unwrap().http.bind, "not-a-bind");
    }

    #[tokio::test]
    async fn rejects_unsupported_config_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        tokio::fs::write(&path, br#"{"version":2}"#).await.unwrap();
        make_private(&path);
        assert!(
            load(&path)
                .await
                .unwrap_err()
                .contains("unsupported configuration version")
        );
    }

    #[tokio::test]
    async fn rejects_account_identities_the_vault_or_cli_cannot_represent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for (id, label) in [
            ("a".repeat(256), "safe".to_owned()),
            ("unsafe\u{202e}id".to_owned(), "safe".to_owned()),
            ("safe".to_owned(), "unsafe\u{2066}label".to_owned()),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "accounts": [{"id": id, "provider": "claude", "label": label}]
            }))
            .unwrap();
            tokio::fs::write(&path, bytes).await.unwrap();
            make_private(&path);
            assert!(load(&path).await.is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reads_once_without_following_links_or_accepting_writable_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let link = directory.path().join("config.json");
        tokio::fs::write(&target, br#"{"version":1}"#)
            .await
            .unwrap();
        symlink(&target, &link).unwrap();
        assert!(load(&link).await.is_err());

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&target).await.is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&target).await.is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load(&target).await.unwrap().version, CONFIG_VERSION);

        let oversized = directory.path().join("oversized.json");
        tokio::fs::write(&oversized, vec![b' '; MAX_CONFIG_BYTES as usize + 1])
            .await
            .unwrap();
        assert!(load(&oversized).await.is_err());
    }
}
