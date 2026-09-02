# Ullage

Ullage is a local daemon and CLI that inspects subscription usage for Claude,
ChatGPT, Grok, and Cursor. A single `ullage` binary hosts the daemon and talks
to it over a private local control socket or named pipe. Credentials stay in the
platform credential store by default; configuration never contains secrets.

## Install

Build from this workspace with Rust 1.85 or newer:

```sh
cargo build --release -p ullage-app
```

The binary is `target/release/ullage`. There is no installer or package in this
release. Place the binary on your `PATH` if you want the user-level service
commands to find it at a stable location.

## Ullage Mac

Ullage Mac is an Apple Silicon menu bar client for macOS 14 or newer. Building
it requires Xcode with the Swift 6 toolchain. It is a separate Swift package
under `apps/ullage-mac`, not part of the Cargo workspace.

Build the application bundle from the repository root or the package directory:

```sh
make mac
make -C apps/ullage-mac bundle
```

Both commands create `apps/ullage-mac/build/Ullage.app`. Open the normal client,
or run it with the bundled fixture data:

```sh
open apps/ullage-mac/build/Ullage.app
open apps/ullage-mac/build/Ullage.app --args --mock
ULLAGE_MOCK=1 apps/ullage-mac/build/Ullage.app/Contents/MacOS/UllageMac
apps/ullage-mac/build/Ullage.app/Contents/MacOS/UllageMac --dump
apps/ullage-mac/build/Ullage.app/Contents/MacOS/UllageMac --dump --mock
```

The popover is 360 points wide, in both light and dark appearance. On macOS 26
it is a single slab of Liquid Glass in a transparent panel, so the desktop and
windows it covers show through it and bend at its edges; the header and tab
row float and the cards scroll underneath them. macOS 14 and 15 open a
standard popover on a tinted backdrop of large oxblood and blue shapes, with
frosted cards over it. Its header
repeats what the menu bar mark is tracking, as a filled vessel beside the
percentage (its surface wobbles while the popover is open, at a fixed level,
under the same conditions that let the menu bar liquid move), the account it came from, that account's soonest reset, and the
runner-up account. Account tabs are a row of pills that wraps onto
further rows rather than scrolling, so every account stays clickable
without a sideways gesture; an account with a row in its lowest two tiers
carries a colored dot. Every account is a card
titled by its provider mark, plan, and badges, and each row keeps the ten-cell
bar filled from the right and colored by tier. Provider marks are the app's own
simplified drawings, not the companies' official logos.

The menu bar mark and application icon share one Swift/CoreGraphics drawing
implementation in the `UllageMac` executable. The menu bar variant uses
dedicated geometry for legibility at 18 points. Its liquid is a solid fill
with a wavy surface, and a faint bar just below the cavity top marks the full
level. The floor of the liquid is, by default, the lowest remaining ratio
across enabled accounts with countable Overview remaining; Settings can pin
one Overview row instead ("Liquid tracks"), and a pinned row that drops out
of a refresh falls back to the lowest remaining until it returns. The liquid breathes
in a forty-second loop from a full vessel down to that floor and back up,
eased so the turnarounds are smooth. A light wobble runs at about 10 fps while
the Mac is on AC power and Reduce Motion is off, and the app holds a
user-initiated activity during that time so App Nap does not throttle the
timer. Battery power, Reduce Motion, or turning off "Animate liquid" in
Settings holds the liquid still at the floor level with a flat surface. The
app polls the daemon every 30 seconds from launch, whether or not the popover
is open, so the mark shows live data without a click. The accessibility
description reports the floor account and its percentage. Overview uses a provider-specific
catalog: ChatGPT 5h and weekly windows plus reset credits, Claude 5h and Fable, Cursor
auto and api, and Grok usage and GrokBuild. Each account tab can show or hide any progress row on Overview with an eye
control; rows without a remaining bar cannot be toggled. Catalog rows start
visible, other progress rows start hidden. Hidden rows also drop out of the
menu bar pin list and the default lowest-remaining floor. Hiding every visible
row leaves an Overview hint pointing back to the account tabs. Unknown providers keep every window
in the shortest available time tier. A reached limit in those retained windows
takes precedence as 0% for that account, so the liquid empties completely at
the bottom of each breath. After the first refresh, an exclamation mark
replaces the liquid when no usable quota is available or the connection enters
an error state, and animation stops. The glass application icon keeps a static
filled liquid level and has six built-in palettes: Amber,
Oxblood, Propellant, Copper, Paper, and Plum; Oxblood is the default. At launch
the app applies the stored `iconPalette` preference (or Oxblood when unset) to
the icon used by the About panel and system dialogs. Finder and Launchpad use
the signed `AppIcon.icns`, so that palette is selected at build time instead.
During `bundle`, the executable renders the standard ten-file
`build/AppIcon.iconset`, `iconutil` converts it to `AppIcon.icns`, and the
build copies that file into the application bundle. No source bitmap or SVG
asset is required. Set the build palette with `ICON_PALETTE`, for example:

```sh
make -C apps/ullage-mac bundle ICON_PALETTE=paper
```

The client reads the server URL from its Settings panel and defaults to
`http://127.0.0.1:7878`. Enable the daemon's HTTP interface as described under
Configuration, create a one-use code with `ullage device pair`, and enter that
code in Settings as six single-character fields (`XXX-XXX`). Paste accepts
values with or without the hyphen, mixed case, and incidental whitespace; the
client normalizes to six uppercase alphanumeric characters before pairing. The
client sends its hostname, exchanges the code for a per-device token, and stores
the token in the macOS Keychain rather than `UserDefaults`.

`Launch at Login` is available from the status-item menu when Ullage is running
from the application bundle. Copy `Ullage.app` to `/Applications` or
`~/Applications` before enabling it so the registered path remains stable.

`--dump` fetches the same accounts and usage projection as the menu bar UI and
prints it without starting the AppKit application loop. Its Overview section
uses the same provider-specific catalog as the popover, and a daemon dump
omits rows hidden in Settings; `--dump --mock` always prints the full catalog.
The per-account sections continue to show all windows. `--render-iconset DIR
[--palette KEY]` likewise renders build assets without starting that loop and
defaults to Oxblood. `--mock` and
`ULLAGE_MOCK=1` remain available for demonstrations with bundled fixtures.
Intel Macs are not supported. By default, `bundle` keeps the local-development
behavior and applies an ad-hoc signature without a timestamp. To create a
hardened-runtime bundle signed for distribution, pass the Developer ID identity:

```sh
make -C apps/ullage-mac bundle \
  SIGN_IDENTITY="Developer ID Application: <name> (<TEAMID>)"
```

To sign, submit the bundle to Apple's notary service, staple the ticket, and
create `build/Ullage-<VERSION>.zip`, use a Keychain profile previously stored
with `xcrun notarytool store-credentials`:

```sh
make -C apps/ullage-mac notarize \
  SIGN_IDENTITY="Developer ID Application: <name> (<TEAMID>)" \
  NOTARY_PROFILE=ullage-notary
```

The same operations can run on the configured remote Mac. Signing credentials
never cross SSH: create a long-lived tmux session once from Terminal in the Mac
GUI login session, then set the session name, identity, and profile locally:

```sh
# Run once in Terminal on the Mac. macOS sleep does not accept "infinity";
# a large second count keeps the hold window alive.
tmux new-session -d -s <gui-session> -n _hold -- sleep 2147483647

export ULLAGE_MAC_GUI_TMUX_SESSION=<gui-session>
export ULLAGE_MAC_SIGN_IDENTITY="Developer ID Application: <name> (<TEAMID>)"
export ULLAGE_MAC_NOTARY_PROFILE=ullage-notary
apps/ullage-mac/scripts/remote.sh sign
apps/ullage-mac/scripts/remote.sh notarize
```

`remote.sh` requires `ULLAGE_MAC_SSH` as before. It synchronizes the package,
runs signing in a temporary window of the GUI-created session, waits up to 30
minutes by default, and copies the notarized zip back into the local `build/`
directory. Set `ULLAGE_MAC_SIGN_TIMEOUT` to a positive number of seconds to
change that limit. A session created over SSH does not inherit the GUI login
security context and therefore cannot reliably access the unlocked login
Keychain or the notarytool profile.

### Connecting to the daemon

Set `http.enabled` to `true` in the daemon configuration, restart the daemon,
and create a short-lived pairing code over the private control channel:

```sh
ullage daemon stop
ullage daemon start
ullage device pair
```

Open Ullage Mac Settings, leave the default server URL as
`http://127.0.0.1:7878` (or use `http://localhost:7878`), type or paste the
displayed pairing code into the six OTP fields (`XXX-XXX`), and select **Pair**.
The client sends its hostname as the device name, receives the device token once,
and stores it in the current macOS user's Keychain. Settings then shows the
paired device name and local pairing time, locks the server URL, and hides the
pair-code fields; select **Unlock** to edit the URL or pair again, and **Lock**
to discard those edits. A successful re-pair locks the panel again. Pairing
codes are one-use, expire after 300 seconds, and a new code invalidates the
previous one. Use
`ullage device list` to inspect active devices and
`ullage device revoke <DEVICE_ID>` to revoke one without affecting the others.

The Mac client accepts the same literal address classes as the daemon:
loopback, Tailscale (`100.64.0.0/10` and `fd7a:115c:a1e0::/48`), RFC 1918, and
IPv6 ULA. It rejects domain names, public and link-local addresses, HTTPS, and
URLs containing userinfo, query, or fragment data. Tailscale traffic stays
inside its encrypted WireGuard tunnel; Ullage does not add TLS, so a device
token sent over a private LAN is plaintext. On the first connection to a daemon
away from this Mac, macOS requests Local Network access. Denying that permission
makes pairing and later connections fail until access is enabled in System
Settings.

As an alternative, keep the HTTP listener on loopback and forward it over SSH.
Run either a local forward from the Mac:

```sh
ssh -N -L 7878:127.0.0.1:7878 daemon-host
```

or a reverse forward from the daemon machine to the Mac:

```sh
ssh -N -R 7878:127.0.0.1:7878 mac-host
```

Do not enable `GatewayPorts`; the daemon permits only loopback, Tailscale, and
private LAN bind addresses and rejects Host values other than its allowlist.

Default paths:

| Platform | Config | State | Control |
|---|---|---|---|
| Linux | `$XDG_CONFIG_HOME/ullage/config.json` or `~/.config/ullage/config.json` | `$XDG_STATE_HOME/ullage/state.json` or `~/.local/state/ullage/state.json`; paired devices in `devices.json` beside that file | `$XDG_RUNTIME_DIR/ullage/control.sock` |
| macOS | `~/Library/Application Support/Ullage/config.json` | `~/Library/Application Support/Ullage/state.json`; paired devices in `devices.json` beside that file | `$TMPDIR/ullage-<uid>/control.sock` |
| Windows | `%APPDATA%\Ullage\config.json` | `%LOCALAPPDATA%\Ullage\state.json`; paired devices in `devices.json` beside that file | `\\.\pipe\ullage-<user-scope>` |

Overrides: `ULLAGE_CONFIG_FILE`, `ULLAGE_STATE_FILE`, `ULLAGE_CONTROL_SOCKET`
(Unix), `ULLAGE_CONTROL_PIPE` (Windows).

Optional file-credential directory, used only when `credentials.file_fallback`
is true and the native store is unavailable: Linux
`$XDG_DATA_HOME/ullage/credentials` or `~/.local/share/ullage/credentials`;
macOS `~/Library/Application Support/Ullage/credentials`; Windows
`%LOCALAPPDATA%\Ullage\credentials`.

## Configuration

Version 1 JSON. Unknown fields, duplicate account IDs, zero timings, symbolic
links, and files larger than 1 MiB are rejected. Secret material is not part of
the schema. If the file is missing, Ullage uses built-in provider endpoints and
an empty account list.

```json
{
  "version": 1,
  "daemon": {
    "maximum_concurrency": 8,
    "default_provider_concurrency": 2,
    "provider_limits": []
  },
  "providers": {},
  "credentials": {
    "file_fallback": false
  },
  "http": {
    "enabled": false,
    "bind": "127.0.0.1:7878",
    "allowed_origins": [],
    "probe_min_interval_seconds": 60
  },
  "accounts": [
    {
      "id": "claude-work",
      "provider": "claude",
      "label": "work",
      "enabled": true,
      "interval_seconds": 300,
      "timeout_seconds": 30,
      "jitter_seconds": 5,
      "backoff_initial_seconds": 30,
      "backoff_maximum_seconds": 1800
    }
  ]
}
```

`provider` must be one of `claude`, `chatgpt`, `grok`, or `cursor`. Provider
OAuth and billing endpoints are compiled in and cannot be redirected here.

`credentials.file_fallback` is off by default. Ullage then uses only the
platform credential store (macOS Keychain, Windows Credential Manager, or Linux
Secret Service). On a machine without Secret Service, authentication fails with
a distinct error; re-run with `--diagnose` to see that this host has no Secret
Service and that file fallback is enabled by setting
`credentials.file_fallback` to `true`.

When the switch is on, Ullage still prefers the native store if that backend is
available. Only when the native backend reports itself unavailable does Ullage
create the platform file-credential directory above. Credentials in that
directory are stored as plaintext. Any process that can read the current user's
files can read them. The directory is created private (`0700` / current-user
DACL); a permission check failure stops the daemon instead of silently
downgrading. Old configuration files that omit the `credentials` object still
load with the switch off.

`http.enabled` is false by default. While it is off the daemon does not listen
on any TCP port. When enabled, `http.bind` accepts `auto:<port>` or one explicit
loopback, Tailscale, or private LAN address. For example, `auto:7878` discovers
all eligible local addresses and listens on each of them, while
`<tailscale-ipv4>:7878` or `<lan-ipv4>:7878` keeps the single-address behavior.
Wildcard, link-local, multicast, and public addresses refuse to start and name
`http.bind`. The server exposes this HTTP API:

```text
POST /v1/pair
GET  /v1/status
GET  /v1/providers
GET  /v1/accounts
GET  /v1/accounts/{id}
GET  /v1/usage?account={id}
POST /v1/accounts/{id}/probe?wait=false
```

`/v1/usage` reads cached snapshots and does not contact providers. Probe
requests for the same account within `http.probe_min_interval_seconds`
(default 60) return `429` with `Retry-After`. Authentication, account
mutation, and workspace commands stay on the private control socket.

Every route except `POST /v1/pair` and `OPTIONS` requires `Authorization:
Bearer <device_token>`. Pairing accepts JSON such as
`{"pair_code":"ABC-DEF","device_name":"client-host"}` and returns the
device ID, sanitized name, and a 256-bit base64url device token. That response
is the only time the raw token is exposed. The six-character code uses the
alphabet `23456789ABCDEFGHJKMNPQRSTVWXYZ`, is case-insensitive on input, and
accepts the hyphen only in the displayed position or with the hyphen omitted.
It expires after 300 seconds, succeeds once, is replaced by the next generated
code, and is invalidated after five failed validations. Pair attempts are also
limited to one per source IP per second; excess attempts return `429` with
`Retry-After`.

`devices.json` is stored beside the state file with current-user-only access
(`0600` / protected DACL). Each active record contains a 12-character device
ID, sanitized name, SHA-256 token hash, creation time, and last-seen time; it
never contains the raw token. Authentication hashes the presented token and
compares it with every active record in constant time without returning early.
Last-seen writes are limited to once per device per 60 seconds. A corrupt or
unsafe device file refuses daemon startup and is never repaired in place. The
legacy `http-token` file is ignored and is not deleted automatically.

The HTTP server
accepts only Host values `127.0.0.1:<port>`, `localhost:<port>`, and the actual
listener addresses; any IPv6 listener additionally enables `[::1]:<port>`.
`http.allowed_origins` is empty by default:
matching origins are echoed with `Vary: Origin`; unmatched origins get no
CORS headers. The server never returns `Access-Control-Allow-Origin: *` or
`Access-Control-Allow-Credentials: true`. In `auto` mode, startup logs one
`http.bind listening <addr> (<class>)` line per listener. If no Tailscale or LAN
address is initially available, discovery retries for up to 60 seconds and then
starts with loopback. A failed loopback bind stops startup; a failed non-loopback
bind emits a warning and is skipped. Remote access may use a direct Tailscale or
LAN address, or an SSH tunnel. Ullage does not offer TLS. Tailscale traffic is
encrypted by WireGuard, but LAN traffic and its device token are plaintext.

Error mapping is stable: missing or invalid Bearer tokens are `401`, unknown
routes `404`, illegal parameters `400`, `AccountNotFound` `404`,
`AuthenticationInvalid` `409`, provider or probe `RateLimited` `429` with
`Retry-After`, `Timeout` `504`, and `Storage` `500`. Pairing additionally uses
`400 bad_request`, `401 pair_code_invalid`, `405`, `413`, and `429`. Response
bodies stay sanitized unless `?diagnose=1` is set. Request bodies larger than
1 MiB, pairing bodies larger than 4 KiB, or bodies that stall past the read
timeout are rejected without affecting other connections.

## Daemon lifecycle

Detached process. The command returns once the daemon is ready:

```sh
ullage daemon run
```

User-level service (current login session only; no system Administrator
service):

```sh
ullage daemon install
ullage daemon start
ullage daemon status
ullage daemon stop
ullage daemon uninstall
```

`install` / `uninstall` manage the startup entry only. Configuration,
credentials, snapshots, and logs remain. `status` reports a live daemon when
the authenticated local endpoint is reachable; otherwise it distinguishes
installed-but-stopped from not-installed. Live table output includes
`CREDENTIAL_BACKEND` (`linux_secret_service`, `macos_keychain`,
`windows_credential_manager`, `file_fallback`, or `other_platform`). JSON uses
the same identifiers on `payload.credential_backend`.

Linux uses a systemd user unit, macOS a LaunchAgent, and Windows a current-user
Task Scheduler task. See `docs/architecture.md` for platform path and permission
details.

## Devices

Use the private local control channel to pair and administer HTTP API clients:

```sh
ullage device pair
ullage device list
ullage device revoke <device-id>
```

`device pair` prints a one-use code and its expiry, then tells you to enter the
HTTP API address and code in the client. The code expires after 300 seconds;
creating another code immediately invalidates the previous one. `device list`
shows only the device ID, name, creation time, and last-seen time. It never
prints device tokens or token hashes. `device revoke` takes effect immediately
and does not prompt for confirmation.

## Accounts

```sh
ullage provider list
ullage account add claude --label work
ullage account list
ullage account show <account-id>
ullage account enable <account-id>
ullage account disable <account-id>
ullage account label <account-id> [label]
ullage account remove <account-id>
```

The same provider may have multiple accounts. Credentials and snapshots are
isolated per account. `account label` renames an account in place; omit the
label to clear it. Interactive login also asks for a label after a successful
authentication.

## Authentication

Interactive login needs a terminal (stdin and stderr). It selects a provider,
creates or reuses an account, prints the authorization URL, waits for the
callback value the provider asks for (or polls a device-code flow), verifies
the stored credential, then asks for an account label:

```sh
ullage auth login
ullage auth login claude
```

You do not need the internal account ID or `ULLAGE_AUTH_CODE`. Providers
describe what to paste (Claude: the full callback URL or `code#state`;
ChatGPT: the `code` query value; Cursor: an API key, typed without echo).
Grok's device-code flow has nothing to paste.

Scripts keep the two-step path:

```sh
ullage auth login <provider> --account <account-id>
ullage auth complete <provider> --account <account-id> <flow-id>
ullage auth status <provider> --account <account-id>
ullage auth logout <provider> --account <account-id>
```

`auth complete` reads the authorization code from `ULLAGE_AUTH_CODE` by default
(or `--authorization-code-env`). Tokens, codes, and API keys are not placed in
process arguments. Interactive login reads them from stdin instead.

`--diagnose` (or `ULLAGE_DIAGNOSE=1`) shows sanitized partial-failure scope
and category on `show` and `probe`. On authentication and probe command
failures it also asks the daemon to attach the provider's own error text.
Default error output is still a stable kind. Without that opt-in, a diagnostic
on the daemon response is rejected as invalid.

ChatGPT workspace selection:

```sh
ullage workspace list chatgpt --account <account-id>
ullage workspace select chatgpt --account <account-id> <workspace-id>
```

## Probe and show

```sh
ullage probe <account-id>
ullage probe <account-id> --no-wait
ullage show <account-id>
ullage show --all
```

`probe` queries the provider and persists a snapshot. `show` reads persisted
snapshots and does not call the provider.

Table output defaults to a readable summary: one line per usable measurement,
with the window, the metric, how much quota is left, when the window resets,
and a ten-cell progress bar as the last column. A row with nothing left reads
`used up` rather than `remains 0%`, which looks like a measurement that came
back empty, and a row under half a percent reads `remains <1%` rather than
rounding down to that same zero. Resets stay in hours and minutes for up to two
days (`resets in 33h30m`), since `in 1d` hides whether the wait is 25 hours or
47; past that they are given in days.

Two kinds of provider bookkeeping never become summary rows. The status booleans
`allowed`, `limit_reached`, `has_credits`, `unlimited`, `on_demand_enabled`, and
`enabled` are hidden as rows, but never as state: a reached limit becomes a
`! limit reached` line, unmetered credits become a `credits unlimited` row, and
a switched-off feature is marked `(off)` on the amount it applies to, or given
its own `disabled` row when the provider reported no such amount. Cursor's
`included_spend` and `bonus_spend` are excluded from the mapping
unconditionally, including when `total_spend` is absent — they are not a state,
only a second breakdown of the money `total_spend` already reports.

Both kinds still appear in the raw table, which you reach with `--raw` or,
without it, when no measurement survives the mapping: the account block then
falls back to the raw table with a note, and still carries the stale, limit,
and partial notices.

```text
==== ACCOUNT claude-work (claude · pro) ====
updated 2m ago
5h           usage  remains 97%  resets in 3h56m  [##########]
weekly       usage  remains 89%  resets in 5d15h  [-#########]
Weekly Opus  usage  remains 89%  resets in 5d15h  [-#########]
```

Global output flags: `--output table|json|pretty-json`, `--color auto|always|never`,
`--raw`, and `--reveal`. `--color` defaults to `auto`, which colors table stdout
when it is a terminal and `NO_COLOR` is unset or empty. JSON and pretty-json
output is never colored. `--raw` replaces the summary with the untranslated
provider table (`WINDOW`, `MEASUREMENT`, `USED`, `LIMIT`, `UNIT`, `RESETS_AT`);
it affects table output only and is a no-op for `--output json` and
`--output pretty-json`. Without `--reveal`, account labels, auth URIs, flow IDs,
and similar personal values are replaced with `[redacted]`. `--raw` does not
change what is redacted. Error details stay redacted even with `--reveal`.

## JSON schema

Successful JSON is a tagged `ControlResult`. Compact `ullage --output json show
<account>` looks like:

```json
{
  "result": "snapshots",
  "payload": [
    {
      "account_id": "claude-work",
      "usage": {
        "outcome": "complete",
        "data": {
          "provider": "claude",
          "account_label": "[redacted]",
          "plan": "pro",
          "subscription_expires_at": null,
          "observed_at": "2026-08-27T12:00:00Z",
          "windows": [
            {
              "window": { "kind": "five_hours" },
              "resets_at": "2026-08-27T17:00:00Z",
              "measurements": [
                {
                  "name": "tokens",
                  "used": 12.0,
                  "limit": 100.0,
                  "unit": { "kind": "tokens" }
                }
              ]
            }
          ]
        }
      },
      "last_success_at": "2026-08-27T12:00:00Z",
      "stale": false,
      "last_error": null,
      "last_error_at": null
    }
  ]
}
```

Window `kind` values are `five_hours`, `weekly`, `monthly`, or
`{"kind":"other","id":"...","label":"..."}`. A missing 5h or weekly window is
omitted; it is never filled with synthetic zeros. `limit` is omitted or `null`
when the vendor reports no cap. `subscription_expires_at` is `null` when the
provider has no expiry.

Errors are written to stderr. Table output uses `error: <kind>` on the first
line. When the CLI can suggest a fix without contacting the daemon, it adds a
static `hint:` line (for example `daemon_unavailable` or
`provider_registry_error`). Parse mistakes print clap's own message: missing
subcommands show that layer's full help; unknown flags, missing arguments, and
invalid enum values include the parameter name and, when available, a
did-you-mean suggestion or the allowed values. Those messages are sanitized
before output and never echo terminal control characters. Recognized option
names such as `--method` or `--account` are named in the hint; positional
arguments and unrecognized flags use a generic static message instead.

JSON and pretty-json use the same envelope with optional fields:

```json
{"status":"error","error":{"kind":"usage","message":"..."}}
```

`message` carries parse-error text when present. `hint` carries static guidance
for selected runtime kinds. Omitted fields are not serialized. JSON never
includes ANSI color sequences. `--help`, `-h`, `help`, and `--version` stay on
stdout and exit `0`. Parse and usage mistakes exit `64`.

Example runtime error:

```json
{"status":"error","error":{"kind":"timeout"}}
```

Partial usage uses `"outcome":"partial"` plus `failures`. CLI exit status `2`
means partial success.

JSON and pretty-json always carry this raw `ControlResult`. `--raw` does not
change their structure or their bytes, so parsers built on this schema keep
working whether or not the flag is passed.

Device commands follow the same tagged shape. `device pair` returns
`{"result":"pair_code","payload":{"code":"ABC-DEF","expires_at":"..."}}`,
`device list` returns `{"result":"devices","payload":[...]}`, and a successful
revoke returns `{"result":"ack"}`. Device list payloads contain no token or
token-hash field.

## Live credentials

This release has no live-credential tests. `cargo test` never sends user
credentials or paid model requests to vendors. If a later suite adds live
smoke, it must be explicit opt-in, must not log account usage numbers, and
must not send paid model requests.

## Documentation

- `docs/architecture.md` — crate graph, storage, hosting, and security
  boundaries
- `docs/development.md` — provider extension, vendor DTO compatibility,
  security, and pre-release checks
