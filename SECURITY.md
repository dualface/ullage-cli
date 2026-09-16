# Security Policy

## Reporting a Vulnerability

Report security issues to dualface@gmail.com.

Do not open a public GitHub issue for problems that involve credentials,
authentication, pairing tokens, or the local control endpoint.

This project stores provider credentials in the platform credential store
(or an opt-in plaintext fallback). Please include the affected command or
HTTP route, the Ullage version, and whether `--diagnose` was required to
see the failure. Do not attach live tokens, refresh tokens, or pairing
codes.
