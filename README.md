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

The menu bar mark and application icon share one Swift/CoreGraphics drawing
implementation in the `UllageMac` executable. The menu bar variant uses
dedicated geometry for legibility at 18 points, while the application icon
keeps its full-color geometry. During `bundle`, the executable renders the
standard ten-file `build/AppIcon.iconset`, `iconutil` converts it to
`AppIcon.icns`, and the build copies that file into the application bundle. No
source bitmap or SVG asset is required.

The normal client reads the server URL from its Settings panel. The URL must be
an HTTP loopback address and defaults to `http://127.0.0.1:7878`. Enable the
daemon's HTTP interface as described under Configuration, print its bearer
token with `ullage http token`, and paste that token into Settings. The client
stores it in the macOS Keychain rather than `UserDefaults`.

`Launch at Login` is available from the status-item menu when Ullage is running
from the application bundle. Copy `Ullage.app` to `/Applications` or
`~/Applications` before enabling it so the registered path remains stable.

`--dump` fetches the same accounts and usage projection as the menu bar UI and
prints it without starting the AppKit application loop. `--render-iconset DIR`
likewise renders build assets without starting that loop. `--mock` and
`ULLAGE_MOCK=1` remain available for demonstrations with bundled fixtures.
Intel Macs are not supported. The bundle receives an ad-hoc signature for local
use; it has no Developer ID signature and is not notarized.

### Connecting to the daemon

Set `http.enabled` to `true` in the daemon configuration, restart the daemon,
and obtain its bearer token without copying it into a script or log:

```sh
ullage daemon stop
ullage daemon start
ullage http token
```

Open Ullage Mac Settings, leave the default `http://127.0.0.1:7878` server URL
(or use `http://localhost:7878`), paste the token, and select **Save** or
**Test connection**. The token is stored in the current macOS user's Keychain.

When the daemon runs on another machine, keep the HTTP listener on loopback and
forward it over SSH. Run either a local forward from the Mac:

```sh
ssh -N -L 7878:127.0.0.1:7878 daemon-host
```

or a reverse forward from the daemon machine to the Mac:

```sh
ssh -N -R 7878:127.0.0.1:7878 mac-host
```

Do not enable `GatewayPorts`; Ullage Mac accepts only loopback HTTP URLs and the
daemon rejects non-loopback Host headers.

Default paths:

| Platform | Config | State | Control |
|---|---|---|---|
| Linux | `$XDG_CONFIG_HOME/ullage/config.json` or `~/.config/ullage/config.json` | `$XDG_STATE_HOME/ullage/state.json` or `~/.local/state/ullage/state.json`; HTTP token `http-token` beside that file | `$XDG_RUNTIME_DIR/ullage/control.sock` |
| macOS | `~/Library/Application Support/Ullage/config.json` | `~/Library/Application Support/Ullage/state.json`; HTTP token `http-token` beside that file | `$TMPDIR/ullage-<uid>/control.sock` |
| Windows | `%APPDATA%\Ullage\config.json` | `%LOCALAPPDATA%\Ullage\state.json`; HTTP token `http-token` beside that file | `\\.\pipe\ullage-<user-scope>` |

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
on any TCP port. When enabled, the daemon binds `http.bind` (loopback only;
a non-loopback address refuses to start and names `http.bind`) and serves a
read-only HTTP query API:

```text
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

Requests need `Authorization: Bearer <token>`. Print or rotate the token with:

```sh
ullage http token
ullage http token --rotate
```

The token is a 256-bit value stored as `http-token` next to the state file,
owned by the current user (`0600` / protected DACL). A permission mismatch
refuses to start or rotate rather than repairing the file. The HTTP server
accepts only Host values `127.0.0.1:<port>` and `localhost:<port>` (plus the
actual loopback bind address). `http.allowed_origins` is empty by default:
matching origins are echoed with `Vary: Origin`; unmatched origins get no
CORS headers. The server never returns `Access-Control-Allow-Origin: *` or
`Access-Control-Allow-Credentials: true`. Remote access is expected to use
an SSH tunnel; this release does not offer TLS or non-loopback binds.

Error mapping is stable: missing or invalid Bearer tokens are `401`, unknown
routes `404`, illegal parameters `400`, `AccountNotFound` `404`,
`AuthenticationInvalid` `409`, provider or probe `RateLimited` `429` with
`Retry-After`, `Timeout` `504`, and `Storage` `500`. Response bodies stay
sanitized unless `?diagnose=1` is set. Request bodies larger than 1 MiB, or
that stall past the read timeout, are rejected without affecting other
connections.

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
and a ten-cell progress bar as the last column.

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
