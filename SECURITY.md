# Security Policy

Lambo PHP manages developer machines: it installs runtimes, writes
configuration, and (from M2) spawns processes and edits system trust stores.
We treat that responsibility seriously.

## Reporting a vulnerability

**Do not open a public issue.** Report privately via GitHub Security
Advisories:

<https://github.com/flessan/kink-php-dev/security/advisories/new>

You will receive an acknowledgment within 72 hours. We aim to ship fixes for
confirmed vulnerabilities within 14 days and will credit reporters (unless
they prefer anonymity) in the release notes.

## Supported versions

| Version | Supported |
| --- | --- |
| latest 0.x | ✅ |
| anything older | ❌ |

Until 1.0, only the newest release receives security fixes.

## Threat model highlights

What Lambo promises, and what we consider a vulnerability:

- **No shell injection.** Lambo executes commands with argument vectors,
  never through a user-influenced shell string. If you find a path where
  project names, versions, or config values reach a shell or a process
  argument unvalidated - that's a security bug.
- **Principle of least privilege.** Lambo must never *require* root for its
  daily workflow. The only foreseen elevated operations (M2+) are adding
  trusted local CA certificates and binding ports below 1024 when the user
  explicitly configures them - each through a minimal, auditable helper.
- **Verified downloads (M2+).** Runtime archives are fetched over HTTPS, and
  checksum-verified against a signed manifest before installation.
- **Sandboxed plugins (M5+).** Plugins run with explicitly declared
  capabilities; a plugin that can read/write outside its granted scope is a
  security bug.
- **No telemetry.** Lambo phones nowhere. The update check (when shipped)
  will be opt-in and document every byte it sends.
