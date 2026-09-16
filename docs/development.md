# Development

This document is for contributors extending Ullage. Runtime architecture lives
in `architecture.md`. User commands live in `README.md`.

## Provider extension

`ullage-cli` is the only composition root that links production providers. A
new provider is a workspace crate under `providers/` that depends on
`ullage-core` and, if it authenticates, `ullage-auth`. Shared crates must not
depend on a provider crate.

Required pieces:

1. Vendor DTOs and HTTP/API types stay in the provider crate.
2. Implement `ullage_core::Provider`. `query` returns vendor data;
   `normalize` converts it to `SubscriptionUsage`.
3. Register a per-account factory in `ullage_app::registry_with_credentials`.
   Credential keys are `CredentialKey::new(provider, account_id)`.
4. Declare capabilities honestly. `WorkspaceSelection` is ChatGPT-only in
   phase 1. `SubscriptionExpiry` is only for providers that actually return
   an end time.
5. Keep OAuth and billing URLs as compiled constants. Local configuration
   cannot redirect them.
6. `start_auth` fills `AuthChallenge.input` when the user must paste a
   value back. Device-code flows leave it `None`. Secret values (API keys)
   set `secret: true` so the CLI disables echo.

Do not add a shared “common vendor DTO” crate. The first time two providers
need the same shape, copy the conversion into each crate until a second
stable consumer exists.

## Vendor DTO compatibility

Vendor payloads change without a public contract. Parse permissively, convert
strictly:

- Dynamic windows: map known durations to `five_hours` / `weekly` /
  `monthly`. Anything else becomes `UsageWindowKind::Other`.
- Absence is not zero. If a vendor omits the 5h window, omit it. Do not
  invent `used: 0` or a fake reset time.
- Unlimited quota is `limit: null`, not a sentinel number.
- `subscription_expires_at` is optional. Missing or inapplicable expiry is
  `null`. Do not derive it from token expiry or usage reset.
- Partial vendor success is `QueryOutcome::Partial`. Fatal failures are
  `ProviderError` only.
- Additive unknown JSON fields on vendor payloads should be ignored, not
  rejected, unless they contradict a required field.

Fixture files under `providers/*/tests/fixtures/` are the compatibility
evidence. They must not contain live tokens.

## Security

Assets are account credentials, subscription snapshots, the local control
endpoint, and the user-level service entry.

- Credentials go through `CredentialStore`. Providers never open Keychain,
  Credential Manager, Secret Service, or a fallback file directly.
  `ullage_app::production_registry` is the only production assembler: native
  keyring by default, `FileStore` only when `credentials.file_fallback` is true
  and the native backend is unavailable. Plaintext fallback files are a
  user-visible security downgrade and must stay opt-in.
- `Debug` and `Display` of tokens, refresh errors, and provider HTTP errors
  must not include secret material.
- The daemon sanitizes provider errors before persist and control responses.
  CLI output redacts personal values unless `--reveal` is set. Error details
  stay redacted even with `--reveal`. `--diagnose` or `ULLAGE_DIAGNOSE=1`
  shows sanitized partial-failure scope and category, and is the only way to
  attach the provider's original error text on authentication and probe
  command failures. A diagnostic on any other response is invalid.
- Control sockets and pipes are current-user only. Unix sockets are `0600`
  in a `0700` directory. Windows pipes include a user-scope suffix.
- Panic payloads are discarded (`ullage daemon task panicked`).
- Authorization codes and API keys enter through stdin in interactive login,
  or from the environment in the scripted `auth complete` path. They never
  appear in argv. Providers declare the paste prompt and whether the value
  is a secret on `AuthChallenge.input`; the CLI does not hard-code callback
  formats.
- This release has no live-credential tests. `cargo test` never sends user
  credentials or paid model requests. A later live-smoke suite would have to
  be explicit opt-in, must not log usage numbers, and must not send paid
  model requests.

## Pre-release checks

From the workspace root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git diff --check
```

Also run the available target checks and record gaps:

```sh
rustup target list --installed
cargo check -p ullage-cli --target x86_64-unknown-linux-gnu
cargo check -p ullage-client --target aarch64-apple-darwin
cargo check -p ullage-client --target x86_64-apple-darwin
cargo clippy -p ullage-auth -p ullage-client --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```

Cross-compiling `ullage-cli` for Windows needs a working MinGW linker for
crates such as `ring`. If that toolchain is missing, keep the Windows
`ullage-auth` / `ullage-client` Clippy coverage and state the gap.

Do not add default-suite tests that contact vendor production APIs with
real credentials.

The sibling repository `ullage-mac-app` embeds a release `ullage` binary from
this workspace with `cargo build --locked --release -p ullage-cli`.

## Homebrew tap

`brew install dualface/tap/ullage` installs a GitHub Release archive, not a
source build. Linux archives are produced on Ubuntu 24.04 (x86_64 and
ARM64) so they stay on glibc 2.39, Homebrew's Linux Tier 1 floor.

After a `v*` tag, the Release workflow rewrites
`Formula/ullage.rb` in [dualface/homebrew-tap](https://github.com/dualface/homebrew-tap)
when the `TAP_TOKEN` secret is set (a PAT with `contents:write` on that
repository). `GITHUB_TOKEN` cannot push to another repository.

To repair the formula from a published tag:

```sh
scripts/sync-homebrew-tap.sh v0.1.1
```

`--dry-run --checksums FILE` prints the formula without pushing. The formula
must include `on_linux` / `on_arm` and `on_linux` / `on_intel` blocks; a
Linux-only Intel URL leaves ARM Linuxbrew users without an install.

`post_install` runs `ullage daemon stop`, `install`, and `start` against the
Cellar keg binary. Stop comes first so an upgrade bootstraps the new keg
instead of kickstarting the previously loaded LaunchAgent. Those commands
use `quiet_system`: a missing GUI session or systemd user bus must not fail
`brew install`. Do not add a Homebrew `service do` block; the daemon's own
user-level unit has the path and permission rules.

## WinGet

`winget install Dualface.Ullage` installs the user-scope Inno Setup package
`ullage-x86_64-pc-windows-setup.exe`. WinGet has no Homebrew-style
`post_install` on a portable zip, so the installer is the hook:
`packaging/windows/ullage.iss` `[Run]` entries call `ullage daemon stop`,
`install`, and `start`. Those flags omit `postinstall` / `skipifsilent` so
winget's silent Inno switches still execute them.

The installer defaults to `PrivilegesRequired=lowest` and allows a command-line
override. The manifest declares `ElevationRequirement:
elevatesSelf`, but its custom `/CURRENTUSER` switch forces WinGet installs
into Inno's non-administrative mode. `DefaultDirName={localappdata}\Ullage`
and the scheduled task therefore belong to the account running WinGet. The
override capability also lets the installer enter administrative mode when
explicitly requested. An installation launched under a different
administrator account belongs to that account.
`PrepareToInstall` stops an already-running daemon so the upgrade can
replace `ullage.exe`. `[UninstallRun]` stops then uninstalls the task. The
GitHub Release zip remains a portable copy and is not what WinGet submits.

After a `v*` tag, the Release workflow compiles the `.iss`, uploads the
setup exe, and opens a PR against
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) when the
`WINGET_TOKEN` secret is set (a PAT that can fork that repository and open
pull requests). `GITHUB_TOKEN` cannot.

To repair or submit manifests from a published tag:

```sh
scripts/sync-winget.sh v0.1.2
```

`--dry-run --checksums FILE` prints the three YAML files without opening a
PR. The installer must declare `InstallerType: inno`, `Scope: user`,
`ElevationRequirement: elevatesSelf`, and `/CURRENTUSER`, and point the URL at
`ullage-x86_64-pc-windows-setup.exe`.
