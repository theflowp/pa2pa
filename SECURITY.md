# Security Policy

## Security Model

`pa2pa` is designed around zero-trust networking and explicit cryptographic allowlisting:

- **Identity & Mutual Whitelist**: Every participant is identified by an OpenSSH **Ed25519** keypair. Messages from non-whitelisted SSH fingerprints are rejected locally and (when allowlist sync is enabled) at the relay edge.
- **End-to-End Encryption**: Payloads are encrypted using ephemeral **X25519** Diffie-Hellman (derived from Ed25519 keys) + **XChaCha20-Poly1305** with authenticated headers (AAD), signed by the sender's Ed25519 key, and verified before decryption. The relay never has access to message plaintext or attachments.
- **Replay & Loop Protection**: Outer/inner `message_id` binding, timestamp skew checks, deduplication, and local automatic-turn counting prevent replay and infinite agent-to-agent loops.
- **At-Rest Local Encryption**: Local SQLite message bodies, subjects, attachments, and approval drafts are encrypted using XChaCha20-Poly1305 with a key derived from the local SSH identity (`0600` file permissions on Unix).
- **Local Control Plane Hardening**: The daemon binds exclusively to loopback (`127.0.0.1` / `[::1]`), validates `Host` and `Origin` headers to block DNS rebinding and CSRF, requires a per-run 256-bit `X-Pa2pa-Token` (compared in constant time), and enforces strict CSP without inline dynamic JS handlers.
- **Runner Isolation**: In `auto-read` mode, headless runners default to an empty sandbox directory (`<pa2pa_home>/sandbox`) and are wrapped in OS-level sandboxing (`sandbox-exec` on macOS, `bwrap` on Linux when available) to restrict writes and block reading `~/.ssh`, `~/.aws`, and `~/.gnupg`.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report vulnerabilities privately via GitHub's **Security Advisories** ("Report a vulnerability" in the repository's **Security** tab).

Please include:
- A clear description of the vulnerability and potential impact.
- Minimal reproduction steps or a proof-of-concept.
- Affected crate(s), OS, and version/commit hash.

We aim to acknowledge reports within 72 hours and coordinate a fix before public disclosure.
