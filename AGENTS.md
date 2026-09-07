# Ullage Project Rules

These rules apply to this repository only. They sit above the shared rule
volumes in the priority chain, and below explicit instructions from the user in
the current task.

## Language

- **Code comments must be written in English.** This covers doc comments
  (`///`, `//!`), inline comments, `#[doc]` attributes, and comments inside
  config, shell, and build files.
- **Git commit messages must be written in English.** Subject and body both.
  Write the subject in the imperative mood, keep it under 72 characters, and
  wrap body lines at 72 characters.
- Everything else that ends up in the repository — identifiers, log and error
  messages, tests, README and `docs/` — is English as well, matching the
  existing codebase.
- Conversation with the user is not affected by this rule; reply in whatever
  language the user is using.

## Workflow

- After finishing a change, create a git commit. Do not leave completed work
  sitting uncommitted in the working tree.

## Remote macOS builds

- Place macOS app build artifacts on `pro2026` under `~/ullage-build/`.
- Also copy the final runnable `.app` bundle to `pro2026:~/Desktop/`.
- Building the macOS app always includes notarization: run
  `apps/ullage-mac/scripts/remote.sh notarize` (which signs, submits to Apple's
  notary service, and staples the ticket), not just `sign`. Copy the stapled
  `.app` to `pro2026:~/Desktop/` after notarization succeeds, then quit any
  running `Ullage`/`UllageMac` instance and `open` the Desktop app so the new
  build starts automatically.
- Replacing the bundle does not replace the running daemon. `ullage-daemon` is
  kept alive by launchd through the Login Item, so it goes on serving the
  binary it was started from and the new build talks to stale daemon code.
  After installing, restart it explicitly:
  `launchctl kickstart -k gui/$(id -u)/com.ullage.mac.daemon`, and confirm the
  process start time is current before testing anything that reaches the
  daemon.
