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
template menu bar mark and full-color application icon. `AppPalette` owns the single canonical
definition of the six named palettes and their icon-rendering colors. `UllageMark` consumes those
definitions rather than owning another color table. The template variant uses dedicated
geometry to remain legible at 18 points. Menu bar liquid is a solid fill with a wavy surface;
`UllageKit` exposes per-account Overview remaining levels, the list of pinnable Overview rows
(`menuBarMetricOptions`, ids stable across refreshes), and `menuBarLiquidLevels`, which returns the
pinned row as the single level or every account's level when nothing usable is pinned
(`AppSettings.menuBarMetricID`). Pure animation helpers pick the
lowest level as the floor (quantized to 20 steps for the accessibility value) and breathe the drawn
height continuously from full to that floor and back over a forty-second cosine-eased cycle, with
an approximately 10 fps wobble. While the wobble timer runs the status item controller holds a
`ProcessInfo` user-initiated activity so App Nap does not throttle the timer. On battery power, when
Reduce Motion is enabled, or when the "Animate liquid" setting (`animatesMenuBarLiquid`, default on)
is off, the liquid holds still at the floor with a flat surface and the cycle is parked at its
trough so resuming rises out of the frozen level. `UsageStore` starts polling at launch on a
30-second cadence and keeps polling while the popover is closed; it stops only at quit.
Settings wears the same surface in a standard titled `NSPanel` whose content is `PopoverSurface`
in its `window` placement, with each section on the popover's frosted panel. The standard title bar
owns the window controls and keeps the content below them. The placement is what the surface needs
to know to fill the content area rather than hang off the status item — no pointer, and the flat
backdrop rounds its own corners, which inside an `NSPopover` the frame already does. The window's
backdrop is held still: the panel outlives its own visibility, and a timeline behind a closed window
would keep ticking.

`PopoverController` presents the content two ways, chosen by `liquidGlassIsEnabled(settings:)`:
macOS 26 has to offer Liquid Glass and the `usesLiquidGlass` setting has to want it. Every surface
that would branch on the system version asks that one function instead, and views too deep to hold
the settings object read the same answer from the `usesLiquidGlass` environment value, which
`RootView` writes once. The controller reads it when the popover opens, and tears down the shell it
built last time when the answer changed, so turning the switch off gives the macOS 14 and 15
presentation exactly rather than an imitation of it. With glass it is a transparent, borderless,
non-activating `NSPanel` hung below the status item, and `RootView` applies `PopoverSurface`, a
root-level `glassEffect(.clear)` clipped to `PopoverBubble`: a 22-point rounded rectangle with a
9-point pointer rising from its top edge, since a borderless panel has no frame to draw the arrow
`NSPopover` supplies. Glass in a transparent window samples what is behind the window, so the
desktop and windows the panel covers show through it and refract at its edges, pointer included.
`PopoverController` reports the tip's offset from the panel's centre through
`PopoverPresentation`, so the pointer keeps aiming at the status item after a screen edge has
pushed the panel inward, and `RootView` adds the pointer's height to the height it reports. The clear variant is deliberate: regular glass blurs and tints a plain
window behind the popover (a white page) into a flat grey, while clear keeps its structure visible;
text sits on the frosted panels rather than on the slab, so no dimming layer is needed. `NSPopover` cannot give that, because its frame draws its own material
and blurs everything behind the window to a flat tone before the content view samples it; a scrim,
a structured backdrop and `Glass.clear` inside the popover were each tried and each left the glass
looking like a plain panel. The panel rebuilds the popover's transient behaviour: a global and a
local mouse-down monitor close it on any click outside it (clicks on the status item are left to
the item's own toggle), Escape closes it, and so does another application activating. Below macOS
26 the same `RootView` goes into an `NSPopover` and `PopoverSurface` paints `AtmosphereBackground`
instead: a solid base with two blurred discs and a diagonal streak, anchored to the top and bottom
edges so the header and the last card always have a shape behind them. The base, discs, and streak
are light- and dark-appearance derivations of the current `AppSettings.iconPalette`; the popover
and Settings inject that value through the same environment key. Liquid Glass does not read or tint
itself with the palette. That flat backdrop is also what macOS 26 draws with the switch off. Each
shape keeps its anchor and breathes: it swells by up to 11% and dims
by up to 10%, on a whole number of turns per 45-second loop so the pattern closes on itself and
nothing jumps when the clock comes round, with different seeds keeping the shapes out of step so
the backdrop reads as weather rather than pulsing as one. The swell is what carries the change,
not travel — a 340-point disc under a 14-point blur barely alters the colour anything sits on when
it slides, and sliding a shape that size reads as a shape being moved rather than as light
changing. The drift runs at the menu bar's frame rate,
only while the popover is on screen, and behind the same gate as the menu bar liquid — the
"Animate liquid" setting, Reduce Motion, and AC power — holding where it is rather than snapping
home when it stops. `PopoverChrome.swift` holds
those surfaces plus the `glassPanel` modifier, a layered material with an inset highlight used by
the header, the tab row and the cards on every macOS version (glass nested in the glass slab would
only re-sample the already blurred slab); the selected tab pill, the header's circular buttons and
the probe button use small `glassEffect` shapes behind `if #available(macOS 26.0, *)`.
Both the header and the tab row live in a `ZStack` above the scroll view, which
starts with a spacer the height of the chrome, `LiquidVessel` (the mark's geometry redrawn in SwiftUI with the icon's
oxblood gradient; its surface is the mark's sine wave at a `wavePhase`, or flat when `nil`, and the
hero drives that phase from a `TimelineView` at the menu bar's tick interval and wave speed while
`PopoverPresentation.isShown` is true and `liquidMotionIsAllowed` (the Settings toggle, Reduce Motion
and battery, shared with the status item) permits; the level itself never animates), the simplified `ProviderMark` drawings, and the header. `heroModel` derives that
header from `menuBarLiquidLevels` and `MenuBarLiquidAnimation.floorLevel`, the same call the status
item makes, and takes the row-visibility sets as required arguments so it resolves a pinned row from
the same list Settings offers, so the popover and the mark cannot disagree; it adds the floor account's soonest reset
and the runner-up level. Both appearances are driven from `colorScheme` rather than fixed colors.
Overview uses a provider-specific catalog (ChatGPT 5h, weekly, and reset credits, Claude 5h and Fable,
Cursor auto and api, Grok usage and GrokBuild). Each account tab can show or hide any progress row on
Overview. Catalog rows default to visible (`AppSettings.hiddenOverviewItemIDs`
hides them); other progress rows default to hidden (`shownOverviewItemIDs`
adds them). Rows without a remaining bar cannot be toggled. Visible rows drive
Overview, daemon dump, `menuBarMetricOptions`, and the default floor. Mock dump
ignores those preferences. An empty Overview after hiding every row shows a
hint to restore them from an account tab. Unknown providers keep every window in the shortest
available time tier, and a reached limit in those retained windows takes precedence as 0% for that
account, which makes the floor empty.
The headless dump retains the per-account Overview projection used by account tabs,
while the menu bar breathes down to the lowest of those levels. The mark shows an exclamation
point after the first refresh when no usable ratio remains or the connection is in an
error state, and stops animation. The application icon retains its full-color geometry
and static filled liquid level.
The macOS app's usage progress bars use ten cells of 10% each and fill remaining quota from the
right, including a partial last cell (5% remaining paints half of one cell). Colors are fixed
semantic values selected solely by each row's remaining-percentage tier. A row with nothing left
reads `used up` rather than `remains 0%`, which looks like a failed measurement, and a row under
half a percent reads `remains <1%` rather than rounding down to the same misleading zero. Reset
times between an hour and two days are given as hours and minutes (`resets in 33h30m`), since `in
1d` hides whether the wait is 25 hours or 47; days take over past that and minutes and seconds
below an hour. The bar is not animated:
switching tabs rebuilds the rows, and a ratio sliding toward its value reads as ten cells shifting
on their own rather than as one bar, so each bar states the number it has at that moment. The executable's headless `--render-iconset` command produces the ten
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
preference (default Oxblood). Settings Appearance exposes one picker for all six palettes without
changing the preference key or raw values; a selection immediately updates the runtime icon and
the shared flat Settings surface, and subsequent flat popovers use the same preference. Finder and
Launchpad remain bound to the build-time `AppIcon.icns` palette.

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
