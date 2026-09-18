# Architecture

User commands are in [README.md](../README.md). Contributor checks are in
[development.md](development.md).

Ullage is a Rust workspace with a one-way dependency graph. Shared crates never depend on a
provider implementation, and provider crates do not depend on each other.

```text
ullage-client --+
ullage-cli -----+--> ullage-protocol --> ullage-core --> ullage-auth
                            ^                ^
                            |                |
ullage-daemon --------------+                |
ullage-http -----> ullage-daemon             |
ullage-cli --------------------------------> provider-claude/chatgpt/grok/cursor/opencode/devin/codex2api
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
- `ullage-cli` (library name `ullage_app`): `ullage-auth`, `ullage-client`, `ullage-core`,
  `ullage-daemon`, `ullage-http`, `ullage-protocol`, and all four provider crates. It is the
  single production composition root and builds the `ullage` binary. This is the published
  crates.io package.
- `ullage-client` (library name `ullage_cli`): `ullage-protocol`, plus `ullage-auth` on Windows
  for private service-marker creation using the same protected-DACL primitive as credential and
  snapshot storage.

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
  probe cooldown. Assembled only by `ullage-cli`; device state remains owned by `ullage-daemon`.
- `ullage-client`: command-line client of the local control protocol.
- `ullage-cli`: single executable composition root for the CLI client and daemon process.

The CLI exposes daemon lifecycle, provider, account, authentication, probe,
snapshot, and device-administration commands. `ullage device pair`,
`list`, and `revoke` map directly to the device control commands introduced in protocol
version 9. The
retired `ullage http token` command is not part of the command surface; HTTP
clients obtain a per-device token only by exchanging a one-use pairing code.

## Ullage Mac

The Apple Silicon menu bar client is a separate Swift package in the sibling
repository `ullage-mac-app`. It is not a member of this Cargo workspace. The
client talks to the daemon over local control (protocol v10) or the HTTP query
API, and embeds a release `ullage` binary from this workspace at bundle time.

See that repository's `docs/architecture.md` for the AppKit/SwiftUI shell,
UllageKit DTOs, Login Item helper, and signing layout.

## Dependency rules

1. Provider implementations may depend on `ullage-core` and `ullage-auth`; shared crates may
   not depend on provider implementations.
2. `ullage-cli` is the composition root and is the only crate that links all production providers.
   `ullage daemon run` launches the same `ullage` executable in its private daemon mode.
3. CLI and app communicate through `ullage-protocol`; they do not inspect daemon or provider
   internals.
   The ChatGPT provider derives its workspace from the OAuth token, selects it during login, and
   uses that account-bound selection for probes.
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
- Protocol version 10 adds `SetAccountMetrics` and carries each account's optional display metric
  hide list on account, snapshot, and probe payloads. Filter names are trimmed, case-insensitive,
  deduplicated, and rejected when empty, longer than 128 characters, more than 64 entries, or
  containing control or bidirectional characters; `InvalidAccountMetrics` is the stable error.
  The one-shot `metric=` query on `/v1/usage` and `ullage show --metric` stay keep lists, applying
  the same names in the opposite direction.

## Runtime configuration

The `ullage` composition root loads a versioned JSON document from `ULLAGE_CONFIG_FILE` first.
Without that override, macOS uses `~/Library/Application Support/Ullage/config.json`, Linux and
other Unix systems use `$XDG_CONFIG_HOME/ullage/config.json` or `~/.config/ullage/config.json`, and
Windows uses `%APPDATA%\Ullage\config.json`. Version 1 contains daemon concurrency, optional
initial account selectors, optional `credentials.file_fallback` (default false), and optional
`http` settings (`enabled` default false, `bind` default `127.0.0.1:7878`, `allowed_origins`
default empty, `probe_min_interval_seconds` default 60). Each account selector may also carry an
optional `metrics` list of display metric names; it seeds a newly created account's hide list,
while the persisted value in `state.json` wins for an account that already exists. Provider
OAuth and billing endpoints are
compiled into the adapters and cannot be redirected through local configuration. Unknown
fields, unsupported versions, duplicate account IDs/selectors, unsafe control characters, unknown
providers, invalid metric filters, zero interval/timeout/backoff timing values (a zero jitter disables
jittering and stays valid), symbolic links, and files above 1 MiB are rejected. Secret
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
failures. `AuthStartRequest` accepts an optional `redirect_uri` for browser OAuth
on the local control channel only; the daemon validates loopback HTTP and rejects
everything else, while the HTTP transport strips the field before calling
providers. Older clients omit the field and providers keep their registered
defaults (`http://localhost:1455/auth/callback` for ChatGPT,
`http://127.0.0.1:1456/callback` for Grok, and Anthropic's remote callback
page for Claude). The macOS app is the first client to send the field; for
ChatGPT it passes the address of the loopback endpoint it has already bound
(see "Ullage Mac"). Grok's compiled-in redirect URI is used only when a client
explicitly requests browser OAuth; the app no longer drives that path.
Control protocol version 10 adds `SetAccountMetrics` and the per-account display metric hide list on
account, snapshot, and probe payloads. Version 9 added device pairing and revocation. Version 8 added
`credential_backend` on daemon status. Version 7 added the diagnostics opt-in
(`diagnostics` / `diagnostic`) and `SetAccountLabel`.

Snapshot payloads carry a `stale` flag with failure semantics, not age
semantics: the daemon sets it when a refresh fails after a successful write
and clears it on the next success. Data age is always read from
`last_success_at`.

## HTTP query API

`ullage-http` is an optional second transport beside the Unix socket / Windows named pipe. It is
off unless `http.enabled` is true. The crate maps query routes onto existing `ControlCommand`
values and calls `ControlService::handle()`; pairing calls the same `DeviceStore` through
`ControlService`. `CONTROL_PROTOCOL_VERSION` is 10.

Endpoints: `POST /v1/pair`, `GET /v1/status`, `/v1/providers`, `/v1/accounts`,
`/v1/accounts/{id}`, `/v1/usage` (optional `?account=`), and
`POST /v1/accounts/{id}/probe` (optional `?wait=false`). `/v1/usage` is
`ControlCommand::Show` and does not call providers. Authentication, account mutation, device
administration and workspace control messages are not exposed over HTTP.

Security model:

- When `http.enabled` is true, `http.bind` accepts `auto:<port>` or one explicit loopback,
  Tailscale (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), RFC 1918, or IPv6 ULA address.
  Wildcard, link-local, multicast, and public addresses fail configuration loading and name that
  setting. `auto` enumerates local interfaces and listens on every eligible unique address, always
  including `127.0.0.1`. It retries discovery for up to 60 seconds when only loopback exists, then
  starts with the available set. Explicit non-loopback `EADDRNOTAVAIL` retries keep the same budget.
  Non-loopback bind failures are logged and skipped; if HTTP setup fails outright — including the
  loopback bind — the daemon stays up and serves the control socket only, reporting the error on
  the daemon's stderr (captured in the per-user `daemon-error-*.log` for CLI-launched daemons).
  Startup logs each listener as `http.bind listening <addr> (<class>)`.
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
