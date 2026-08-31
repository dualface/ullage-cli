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

## Remote macOS builds

- Place macOS app build artifacts on `pro2026` under `~/ullage-build/`.
- Also copy the final runnable `.app` bundle to `pro2026:~/Desktop/`.
