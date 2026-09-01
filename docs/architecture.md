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
- `ullage-http`: `ullage-daemon` and `ullage-protocol`. It is an optional local or
  Tailscale HTTP transport over `ControlService::handle()` and does not change the control protocol.
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
- `ullage-http`: loopback, Tailscale, or private-LAN HTTP query and pairing transport with explicit
  or automatic multi-address binding, Host/Origin checks, per-IP pairing limits, and per-account
  probe cooldown. Assembled only by `ullage-app`; device state remains owned by `ullage-daemon`.
- `ullage-cli`: command-line client of the local control protocol.
- `ullage-app`: single executable composition root for the CLI client and daemon process.

The CLI exposes daemon lifecycle, provider, account, authentication, workspace,
probe, snapshot, and device-administration commands. `ullage device pair`,
`list`, and `revoke` map directly to the version 9 device control commands. The
retired `ullage http token` command is not part of the command surface; HTTP
clients obtain a per-device token only by exchanging a one-use pairing code.

## Ullage Mac

`apps/ullage-mac` is a standalone Swift package and is not a member of the Cargo workspace.
Its `UllageKit` target owns the daemon HTTP DTOs, transport client, decoding, ordering, and summary
projection. The `UllageMac` executable target owns the `UsageDataSource` composition boundary and
its daemon and mock adapters, as well as the AppKit application shell, SwiftUI views, refresh
lifecycle, settings, and Keychain access. The application bundle copies the SwiftPM resource bundle
into `Contents/Resources` so both tests and mock mode use the same canonical fixture files.
The executable also owns the shared Swift/CoreGraphics U-vessel drawing implementation used for the
template menu bar mark and full-color application icon. `UllageMark.Palette` owns the six canonical
application-icon palettes and their glass-rendering colors. The template variant uses dedicated
geometry to remain legible at 18 points. Menu bar liquid is a stroked wave at a 20-step height for
the currently displayed enabled account; `UllageKit` exposes per-account Overview remaining levels
and pure animation helpers that rotate accounts every 60 seconds with eased height changes and an
approximately 10 fps wobble. Animation freezes on battery power or when Reduce Motion is enabled.
Overview uses a provider-specific catalog (ChatGPT weekly and reset credits, Claude 5h and Fable,
Cursor auto and api, Grok usage and GrokBuild). Unknown providers keep every window in the shortest
available time tier, and a reached limit in those retained windows takes precedence as 0% for that
account while still participating in rotation.
The headless dump retains the per-account Overview projection used by account tabs,
while the menu bar cycles those same per-account levels. The mark shows an exclamation
point after the first refresh when no usable ratio remains or the connection is in an
error state, and stops animation. The application icon retains its full-color geometry
and static filled liquid level.
The macOS app's usage progress bars use ten cells of 10% each and fill remaining quota from the
right, including a partial last cell (5% remaining paints half of one cell). Colors are fixed
semantic values selected solely by each row's remaining-percentage tier. The executable's headless `--render-iconset` command produces the ten
standard PNG renditions during `bundle`;
`iconutil` converts them to `AppIcon.icns` before the bundle is signed, so the repository does not
carry generated SVG or bitmap icon assets. Bundle signing is
ad-hoc by default. A caller can instead select a Developer ID identity to enable the hardened runtime
and a secure timestamp, then use the `notarize` target to submit, staple, and package the application.
Remote distribution signing runs inside a user-provided tmux session created by the Mac GUI login
session; the remote workflow passes only identity and Keychain profile names, never credentials.

The client accesses daemon data only through HTTP; it does not read Rust state, credentials,
snapshots, or the private control socket directly. Settings accepts the daemon's literal loopback,
tailnet, and private-LAN address classes and rejects domains and URL components that could redirect
credentials. `UllageKit` sends the bearer-free pairing request, then the executable stores the returned
per-device token as a generic password in the macOS Keychain with device-local, unlocked-only
accessibility. Settings presents the one-use pair code as six single-character fields with a fixed
middle hyphen (`XXX-XXX`); paste input is normalized to at most six uppercase alphanumeric
characters before the request is sent. A successful pair makes a best-effort attempt to remove the retired shared-token
Keychain entry without discarding the new credential when an old-item ACL denies deletion. Only the
paired device name and local pairing time are kept in `UserDefaults` for display. Non-loopback access declares
`NSLocalNetworkUsageDescription`, so macOS can request Local Network permission with an explanation.
The executable-owned `UsageDataSource` boundary selects either the real `DaemonClient` or bundled mock
fixtures. Daemon HTTP responses and the Mac client both use the version 9 result envelope. Probes
distinguish acknowledged and completed responses. The headless `--dump` path reuses the same summary projection as the menu
bar UI, while `--render-iconset` reuses the executable's canonical mark geometry without entering
the AppKit application loop. Runtime application-icon color follows the stored `iconPalette`
preference (default Oxblood); Settings no longer exposes a palette picker.

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
- Protocol version 9 adds `CreatePairCode`, `ListDevices`, and `RevokeDevice`. Pair-code responses
  contain only the short-lived code and expiry; device-list payloads contain display metadata but
  never a device token or token hash. `DeviceNotFound` is the stable unknown-device error.

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
failures. Control protocol version 9 adds device pairing and revocation. Version 8 added
`credential_backend` on daemon status. Version 7 added the diagnostics opt-in
(`diagnostics` / `diagnostic`) and `SetAccountLabel`.

## HTTP query API

`ullage-http` is an optional second transport beside the Unix socket / Windows named pipe. It is
off unless `http.enabled` is true. The crate maps query routes onto existing `ControlCommand`
values and calls `ControlService::handle()`; pairing calls the same `DeviceStore` through
`ControlService`. `CONTROL_PROTOCOL_VERSION` is 9.

Endpoints: `POST /v1/pair`, `GET /v1/status`, `/v1/providers`, `/v1/accounts`,
`/v1/accounts/{id}`, `/v1/usage` (optional `?account=`), and
`POST /v1/accounts/{id}/probe` (optional `?wait=false`). `/v1/usage` is
`ControlCommand::Show` and does not call providers. Authentication, account mutation, device
administration, and workspace commands are not exposed over HTTP.

Security model:

- When `http.enabled` is true, `http.bind` accepts `auto:<port>` or one explicit loopback,
  Tailscale (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), RFC 1918, or IPv6 ULA address.
  Wildcard, link-local, multicast, and public addresses fail configuration loading and name that
  setting. `auto` enumerates local interfaces and listens on every eligible unique address, always
  including `127.0.0.1`. It retries discovery for up to 60 seconds when only loopback exists, then
  starts with the available set. Explicit non-loopback `EADDRNOTAVAIL` retries keep the same budget.
  A loopback bind failure stops startup; other bind failures are logged and skipped. Startup logs
  each listener as `http.bind listening <addr> (<class>)`.
- Tailscale traffic is encrypted by WireGuard. Private LAN traffic has no transport encryption, so
  its device token is sent in plaintext; Ullage does not add TLS.
- Pairing: the local control channel creates one in-memory code at a time. The code uses
  `23456789ABCDEFGHJKMNPQRSTVWXYZ`, is displayed as `XXX-XXX`, expires after 300 seconds, succeeds
  once, and is invalidated by replacement or five failed validations. Input is case-insensitive
  and may omit the hyphen, but no other character or hyphen position is accepted. Generation uses
  `getrandom` with rejection sampling; comparison is constant-time. `POST /v1/pair` is the only
  bearer-free route other than `OPTIONS`, requires `application/json`, limits bodies to 4 KiB, and
  is rate-limited to one attempt per source IP per second with a bounded expiry-cleaned table.
- Devices: `ControlService` owns one shared `DeviceStore` used by the control and HTTP transports.
  Successful pairing creates a 12-character ID and a 256-bit base64url token, returns the raw token
  once, and persists only its SHA-256 hash. `devices.json` sits beside the state file; Unix uses a
  private parent and `0600`, while Windows uses a protected current-user-only DACL. Updates use a
  private temporary file and atomic replacement. Corrupt, public, symlink/reparse, or otherwise
  unsafe storage refuses startup without repair. Records contain ID, sanitized name, token hash,
  RFC3339 creation and last-seen times, plus an internal revocation tombstone. The legacy
  `http-token` file is ignored and left in place.
- Authentication: every protected request hashes the presented device token and constant-time
  compares it with every non-revoked record without an early return. An empty store never bypasses
  authentication. Successful requests update `last_seen_at`, with at most one file write per device
  per 60 seconds. Revocation preserves a tombstone for idempotency and immediately rejects that
  token; active device listings omit revoked records.
- Host whitelist: `127.0.0.1:<port>`, `localhost:<port>`, and every actual listener address. Any
  IPv6 listener additionally enables `[::1]:<port>`. Other Host values return 403.
- CORS: `http.allowed_origins` defaults to empty. A matching Origin is echoed with `Vary:
  Origin`. Unmatched origins receive no `Access-Control-Allow-*` headers. The server never
  returns `*` or `Access-Control-Allow-Credentials: true`. OPTIONS preflight is supported. CORS
  is not the authorization gate; token and Host checks are.
- Probe cooldown is per account (`http.probe_min_interval_seconds`, default 60) because engine
  single-flight only merges concurrent probes.
- Request bodies over 1 MiB, pairing bodies over 4 KiB, or bodies past the read timeout are rejected
  on that connection only.
- HTTP `/v1` is independent of `CONTROL_PROTOCOL_VERSION`. Status payloads still carry the
  control protocol version.

This release does not offer TLS, public or link-local binds, Cookie authentication,
SSE/WebSocket, or static page hosting. Remote use is direct over Tailscale or a private LAN, or
through an SSH tunnel onto the loopback listener.

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
