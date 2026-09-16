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
