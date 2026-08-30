# Architecture

User commands are in [README.md](../README.md). Contributor checks are in
[development.md](development.md).

Ullage is a Rust workspace with a one-way dependency graph. Shared crates never depend on a
provider implementation, and provider crates do not depend on each other.

```text
ullage-cli ----+
ullage-app ----+--> ullage-protocol --> ullage-core --> ullage-auth
                            ^                ^
                            |                |
ullage-daemon --------------+                |
ullage-http -----> ullage-daemon             |
ullage-app --------------------------------> provider-claude/chatgpt/grok/cursor
```

An arrow points from a consumer to one of its dependencies. The compact diagram shows ownership
direction rather than every composition-root edge; the complete direct workspace dependency list is:

- `ullage-auth`: none.
- `ullage-core`: `ullage-auth`.
- `ullage-protocol`: `ullage-core`, `ullage-auth`.
- each `ullage-provider-*`: `ullage-core`; providers that implement authentication also use `ullage-auth`.
- `ullage-daemon`: `ullage-auth`, `ullage-core`, and `ullage-protocol`; it owns scheduling, persistence, and local
  control transport without choosing production providers.
- `ullage-http`: `ullage-auth`, `ullage-daemon`, and `ullage-protocol`. It is an optional loopback HTTP
  transport over `ControlService::handle()` and does not change the control protocol.
- `ullage-app`: `ullage-auth`, `ullage-cli`, `ullage-core`, `ullage-daemon`, `ullage-http`,
  `ullage-protocol`, and all four provider crates. It is the single production composition root
  and builds the `ullage` binary.
- `ullage-cli`: `ullage-protocol`, plus `ullage-auth` on Windows for private service-marker
  creation using the same protected-DACL primitive as credential and snapshot storage.

## Crate ownership

- `ullage-auth`: provider-independent authentication requests, challenges, states, and credential
  storage/rotation primitives.
- `ullage-core`: unified usage DTOs, provider errors, the typed `Provider` contract, and the
  object-safe provider registry boundary.
- `ullage-protocol`: versioned request and response DTOs for local daemon control.
- `ullage-provider-*`: vendor-specific DTOs, API behavior, and conversion into `ullage-core`
  DTOs. Vendor DTOs must not be moved into a shared crate.
- `ullage-daemon`: local transport and scheduling.
- `ullage-http`: loopback HTTP query transport, bearer token file, Host/Origin checks, and
  per-account probe cooldown. Assembled only by `ullage-app`.
- `ullage-cli`: command-line client of the local control protocol.
- `ullage-app`: single executable composition root for the CLI client and daemon process.

## Ullage Mac

`apps/ullage-mac` is a standalone Swift package and is not a member of the Cargo workspace.
Its `UllageKit` target owns the daemon HTTP DTOs, transport client, decoding, ordering, and summary
projection. The `UllageMac` executable target owns the `UsageDataSource` composition boundary and
its daemon and mock adapters, as well as the AppKit application shell, SwiftUI views, refresh
lifecycle, settings, and Keychain access. The application bundle copies the SwiftPM resource bundle
into `Contents/Resources` so both tests and mock mode use the same canonical fixture files.

The client accesses daemon data only through the loopback HTTP API; it does not read Rust state,
credentials, snapshots, or the private control socket directly. Its bearer token is stored as a
generic password in the macOS Keychain with device-local, unlocked-only accessibility. The current
stage uses the executable-owned `UsageDataSource` boundary with bundled mock fixtures. Real HTTP
behavior and compatibility validation belong to `20260831-mac-http-integration-task`.

## Dependency rules

1. Provider implementations may depend on `ullage-core` and `ullage-auth`; shared crates may
   not depend on provider implementations.
2. `ullage-app` is the composition root and is the only crate that links all production providers.
   `ullage daemon run` launches the same `ullage` executable in its private daemon mode.
3. CLI and app communicate through `ullage-protocol`; they do not inspect daemon or provider
   internals.
   ChatGPT workspace discovery and selection use the same account-bound control path, and the
   selected workspace is persisted by that account's Provider instance.
4. Cross-crate data uses public DTOs. No crate reads another crate's private state or storage.
5. Changes to shared contracts are coordinated in an integration task after parallel provider
   work begins.

## Credential storage boundary

Providers identify credentials with `CredentialKey(provider, account_id)` and use
`CredentialStore`; they never open a platform credential service or fallback file directly. The
stable native-store identity is the `dev.onevoke.ullage.credentials` application namespace plus
the provider and account ID. Windows additionally uses a fixed-length SHA-256 target over that
unambiguous tuple to prevent Credential Manager delimiter aliases, with bounded non-secret metadata
that remains below the platform comment limit. macOS and Linux retain their platform-defined
default target/domain semantics.

`NativeStore` selects macOS Keychain, Windows Credential Manager, or Linux Secret Service at
compile time. Its availability probe distinguishes an absent Linux D-Bus session or Secret Service
provider by its concrete DBus error name from storage access failures. Access failures advise both
unlocking the store and checking the current user's permissions. The native store never selects
another backend. `FileStore` is a
separate constructor requiring an explicit
absolute path, so failure of a native credential service cannot silently create plaintext files.
The composition root may construct `FileStore` only when configuration sets
`credentials.file_fallback` to true and `NativeStore::probe` returns
`Unavailable`. The default remains native-only. When the switch is off and the
native backend is unavailable, credential operations fail with a dedicated
error that names Secret Service and the configuration switch; `--diagnose` on
an authentication or probe command surfaces that text. `ullage daemon status` reports
the selected backend as `linux_secret_service`, `macos_keychain`,
`windows_credential_manager`, `file_fallback`, or `other_platform`.
The fallback opens the filesystem root/volume once, traverses every path component through fixed
parent handles with no-follow semantics, and reopens any created leaf through that same parent. It
rejects symlink/reparse-point entries, verifies ownership and permissions from open handles, then
keeps the fixed capability directory handle for every relative read, create, rename, and delete.
Fixed-length SHA-256 record names support every accepted key without exceeding common filesystem
component limits. It writes through a private temporary file and atomically replaces the destination.
On Windows a new fallback leaf is created with a current-user-only protected DACL in the
`CreateDirectoryW` call itself; an existing directory is accepted only when its fixed handle shows
the same owner-only protected ACL and is never repaired in place. File handles request `WRITE_DAC`,
install the same protected owner-only ACL, and verify it through the fixed handle before credential
data is used.

Stored records carry a generation plus a revision. Delete replaces the secret with a tombstone and
advances the generation; recreating the same key therefore cannot reuse a version observed by an
older refresh. Replacement compares both values, making refresh-token rotation reject stale
writers across logout/re-authentication. Every store handle for the same underlying vault shares a
coordination domain derived from the verified directory handle's filesystem identity, independent
of path spelling. Asynchronous refresh allows only one in-flight provider call per account. Callers
with the same observed version share its complete success or failure; callers with another version
wait and then re-evaluate before starting a later flight. Different accounts can refresh
concurrently. A flight is registered before any backend read, so storage-read failures are shared
too. A cancelled leader releases its waiters with a sanitized synchronization error. Credential
values and provider refresh errors are redacted from `Debug` and `Display` output.

## Contract conventions

- Usage queries return `QueryOutcome<T>` so partial success carries both normalized data and scoped
  failures. This is the only partial-success representation; `ProviderError` contains fatal
  failures only.
- The local protocol wraps provider and registry failures in `ControlError`, preserving unknown
  provider IDs as a stable, serializable response instead of a transport failure.

## Runtime configuration

The `ullage` composition root loads a versioned JSON document from `ULLAGE_CONFIG_FILE` first.
Without that override, macOS uses `~/Library/Application Support/Ullage/config.json`, Linux and
other Unix systems use `$XDG_CONFIG_HOME/ullage/config.json` or `~/.config/ullage/config.json`, and
Windows uses `%APPDATA%\Ullage\config.json`. Version 1 contains daemon concurrency, optional
initial account selectors, optional `credentials.file_fallback` (default false), and optional
`http` settings (`enabled` default false, `bind` default `127.0.0.1:7878`, `allowed_origins`
default empty, `probe_min_interval_seconds` default 60). Provider OAuth and billing endpoints are
compiled into the adapters and cannot be redirected through local configuration. Unknown
fields, unsupported versions, duplicate account IDs/selectors, unsafe control characters, unknown
providers, zero timing values, symbolic links, and files above 1 MiB are rejected. Secret
credential values are not part of the schema and are therefore rejected rather than copied into configuration.
When file fallback is enabled and the native backend is unavailable, plaintext
files are written under `$XDG_DATA_HOME/ullage/credentials` or
`~/.local/share/ullage/credentials` on Linux, `~/Library/Application Support/Ullage/credentials`
on macOS, and `%LOCALAPPDATA%\Ullage\credentials` on Windows. That directory is
an explicit security downgrade: any reader of the current user's files can
recover long-lived tokens. Permission failures on that directory abort rather
than falling back.
Configuration and snapshot files are validated through their opened handles: Unix requires the
current owner and private modes, while Windows requires a protected DACL granting only the current
user access and rejects reparse points. Windows snapshot directories and temporary files receive
that protected DACL at creation, before account labels or usage data are written.

If the file is absent, the built-in provider endpoints and empty account list are used. Mock-server
tests inject Provider factories directly rather than weakening the production configuration boundary.
Interactive `ullage auth login` reads authorization codes and API keys from stdin after the
provider describes what to paste (`AuthInputRequest` on `AuthChallenge`). The scripted
`auth complete` path still reads the authorization code from `ULLAGE_AUTH_CODE` by default
(or the environment variable named with `--authorization-code-env`). Tokens, codes, and API keys
stay out of process arguments. Provider error text stays sanitized on the control channel unless
the client opts in with `--diagnose` / `ULLAGE_DIAGNOSE=1`. That flag shows
sanitized partial-failure scope and category on `show` and `probe`, and
attaches the provider's own error text only on authentication and probe
failures. Control
protocol version 8 adds `credential_backend` on daemon status. Version 7 added the diagnostics
opt-in (`diagnostics` / `diagnostic`) and `SetAccountLabel`.

## Loopback HTTP query API

`ullage-http` is an optional second transport beside the Unix socket / Windows named pipe. It is
off unless `http.enabled` is true. The crate maps HTTP routes onto existing `ControlCommand`
values and calls `ControlService::handle()`; `CONTROL_PROTOCOL_VERSION` stays 8.

Endpoints: `GET /v1/status`, `/v1/providers`, `/v1/accounts`, `/v1/accounts/{id}`, `/v1/usage`
(optional `?account=`), and `POST /v1/accounts/{id}/probe` (optional `?wait=false`). `/v1/usage`
is `ControlCommand::Show` and does not call providers. Authentication, account mutation, and
workspace commands are not exposed.

Security model:

- Bind address must be loopback. A non-loopback `http.bind` fails daemon startup and names that
  setting.
- Token: 256-bit `getrandom`, base64url, stored as `http-token` next to the state file. Unix
  files must be current-user-owned `0600`; Windows files use the same protected DACL primitive as
  credentials and snapshots. Permission mismatches refuse to start or rotate and are not repaired
  in place. Comparison is constant-time. `ullage http token --rotate` replaces the file so a
  running daemon rejects the old token on the next request.
- Host whitelist: `127.0.0.1:<port>`, `localhost:<port>`, and the actual loopback bind address.
  Other Host values return 403.
- CORS: `http.allowed_origins` defaults to empty. A matching Origin is echoed with `Vary:
  Origin`. Unmatched origins receive no `Access-Control-Allow-*` headers. The server never
  returns `*` or `Access-Control-Allow-Credentials: true`. OPTIONS preflight is supported. CORS
  is not the authorization gate; token and Host checks are.
- Probe cooldown is per account (`http.probe_min_interval_seconds`, default 60) because engine
  single-flight only merges concurrent probes.
- Request bodies over 1 MiB or past the read timeout are rejected on that connection only.
- HTTP `/v1` is independent of `CONTROL_PROTOCOL_VERSION`. Status payloads still carry the
  control protocol version.

This release does not offer TLS, non-loopback binds, Cookie authentication, SSE/WebSocket, or
static page hosting. Remote use is SSH tunneling onto the loopback listener.

## User service hosting

`ullage daemon install/start/stop/status/uninstall` manages the daemon in the current user's
session. `status` reports live daemon details when its authenticated local control endpoint is
reachable, otherwise it distinguishes an installed-but-stopped service from a service that is not
installed. Installation never creates a system-wide service and uninstallation first confirms the
daemon has stopped, then removes only the startup entry; configuration, credentials, snapshots,
and logs remain intact.

- Linux installs a private `ullage.service` under `$XDG_CONFIG_HOME/systemd/user` (or
  `~/.config/systemd/user`), enables it for the user's `default.target`, and delegates lifecycle
  operations to `systemctl --user`. The unit uses the absolute executable path, `UMask=0077`,
  `NoNewPrivileges=true`, and the systemd journal.
- macOS installs `~/Library/LaunchAgents/dev.onevoke.ullage.plist`, uses an XML-escaped
  `ProgramArguments` array rather than a shell command, and delegates lifecycle operations to the
  current user's `launchctl gui/<uid>` domain. Logs use the current-user-owned
  `~/Library/Logs/Ullage` directory; a newly created log directory is `0700`, but
  an existing directory is not required to be mode-private. Configuration and
  state use `~/Library/Application Support/Ullage`.
- Windows uses a current-user Task Scheduler task triggered at logon with limited privileges.
  `schtasks.exe` is invoked directly with an argv array; this is deliberately not a Windows Service,
  because creating a system service requires Administrator rights. Start, stop, and uninstall map
  to Task Scheduler run, end, and delete operations. The task name and default named-pipe endpoint
  include a stable SHA-256 scope derived from the current user's SID, so separate local users cannot
  collide or reserve each other's service identity. Each installed task also has a cryptographically
  random nonce recorded in a current-user-only marker under `%LOCALAPPDATA%\Ullage`. The marker uses
  a recoverable two-phase `pending`/`installed` state: retrying install completes either side of an
  interrupted registration, while uninstall safely removes a pending task or marker. First
  installation does not pass Task Scheduler's replacement flag; replacement is allowed only after
  the private installed marker and nonce-bound task definition agree. Other marker/task mismatches
  are explicit errors rather than silently reporting `not-installed`.

Linux and macOS service manifests are installed atomically with mode `0600`; symbolic-link paths,
systemd specifier/environment expansion, XML metacharacters, and control characters in generated
values are escaped or rejected. Unix executable paths must be owned by the current user or root and
must not be symbolic links. Service manifests and macOS log files must be current-user-owned regular
files; their ancestors must be owned by the current user or root and must not be symbolic links.
Group or other write bits on those paths are not an install-time rejection, so a umask of `002` and
cargo's `0775` artifacts can be installed. Windows pins the executable and its immediate parent with
read-share-only handles across task registration, validates higher ancestors with share-compatible
handles, and rejects every reparse-point component. It also rejects `%` expansion in the registered
executable path. Existing macOS log paths must already be current-user-owned, regular, and
non-symbolic-link entries. The generated manifests and Windows task contain only the executable path
and private `__daemon` argument. Runtime configuration remains in the platform configuration
directory, snapshots remain in the platform state/data directory, and credentials remain in the
native credential store unless file fallback is explicitly enabled.
