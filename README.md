# Lambo PHP

> **Release Candidate `0.14.0-rc.1` - The installer is broken and Windows 11
> is not detected.**
> A Windows installer has been created, but running it on Windows 11 causes
> a **SIGSEGV** error:
> `The application was unable to start correctly (0xc000001e) and must be closed.`
> The installer `LamboPHP-Setup.exe` is available in the assets of the
> release. You can download it and manually run the acceptance checklist in
> [docs/windows.md](docs/windows.md).

**A native local PHP development environment.**

One binary that installs and runs PHP, Apache and MariaDB for your projects -
no Docker, no WSL, no hand-edited `httpd.conf` or `php.ini`, and no changes to
your system `PATH`. Windows is a first-class platform; Linux and macOS are
supported wherever the upstream runtimes are.

```
cd your-project
lambo init      # detect the project, write lambo.yml
lambo up        # PHP + Apache + MariaDB, verified before it claims success
                # → http://localhost
lambo down      # stop everything Lambo started, and nothing else
```

[![CI](https://github.com/flessan/kink-php-dev/actions/workflows/ci.yml/badge.svg)](https://github.com/flessan/kink-php-dev/actions/workflows/ci.yml)
[![Release](https://github.com/flessan/kink-php-dev/actions/workflows/release.yml/badge.svg)](https://github.com/flessan/kink-php-dev/actions/workflows/release.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

📖 **[Docs](docs/)** · 🪟 **[Windows guide](docs/windows.md)** ·
🗺️ **[Roadmap](docs/roadmap.md)** ·
💬 **[Discussions](https://github.com/flessan/kink-php-dev/discussions)**

---

## Install

**Windows 10/11** - download `LamboPHP-Setup.exe` from the
[latest release](https://github.com/flessan/kink-php-dev/releases), or use the
portable `LamboPHP-windows-amd64.zip` and put `lambo.exe` anywhere you like.
No administrator rights are needed.

**Linux / macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/flessan/kink-php-dev/main/scripts/install.sh | sh
```

Everything Lambo owns lives under one directory - `%USERPROFILE%\Lambo` on
Windows, `~/.lambo` elsewhere - and `LAMBO_HOME` moves it anywhere, including
onto a USB stick. Details: [docs/installation.md](docs/installation.md).

> [!IMPORTANT]
> **The download catalogue ships without pinned checksums.** Lambo refuses to
> run anything it cannot verify, so `lambo php install` will fail closed until
> you pin a SHA-256 for the releases you want. That is deliberate: a tool that
> executes downloaded binaries should not guess about their integrity. One
> file fixes it - see
> [docs/installation.md#pinning-checksums](docs/installation.md#pinning-checksums).

## What Lambo does

| | |
| --- | --- |
| **PHP** | Detects, downloads, verifies and installs PHP per version; activates one without touching `PATH`; generates `php.ini` |
| **Apache** | Installs it where a build exists, and *generates* `httpd.conf` - you never edit it. Validates the config, then confirms the port actually answers |
| **MariaDB / MySQL** | Installs the server, owns the data directory, initializes it, generates a strong root password, and health-checks over TCP |
| **Projects** | `lambo init` writes a `lambo.yml` from evidence (Laravel, Symfony, CodeIgniter, WordPress, Composer, plain PHP) and states what it found - including what it ruled out |
| **`.env`** | Writes the database keys your app needs without overwriting values you already set |
| **Database UI** | `lambo db open` serves phpMyAdmin - through Apache at `http://localhost/phpmyadmin` when the project is running, on loopback otherwise - and opens it with the connection filled in (never the password in the URL) |
| **Diagnostics** | `lambo doctor` checks the home, transport, catalogue, runtimes, ports, project and services - every finding names the command that fixes it |
| **Offline & mirrors** | Artifacts can come from the official host, a mirror you run, or a local directory. The expected checksum always comes from the catalogue, never from the source - so an offline install gets the same guarantee as a downloaded one |

## Commands

```
lambo init|up|down|restart|status|open|doctor|logs|migrate
lambo php list|list-versions|install|use|current|remove|ini|modules|run|-v
lambo server start|stop|restart|status
lambo db install|install-ui|start|stop|restart|status|create|drop|list|shell|open|credentials
lambo config show|get|set|list|path|edit
lambo workspace list|add|remove
```

Full reference: [docs/commands.md](docs/commands.md).
Something broken: [docs/troubleshooting.md](docs/troubleshooting.md).
How PHP is installed, verified and run: [docs/runtime.md](docs/runtime.md).

## The project file

`lambo init` writes a `lambo.yml` you own, read and commit:

```yaml
# A Laravel project
name: my-shop
php: ~8.3
server:
  kind: apache
  document_root: public
database:
  kind: mariadb
  name: my_shop
```

A plain PHP project gets `document_root: .` and `database: {kind: none}`.
Relative paths only - never `C:\…` - so the same file works on every machine.
Reference: [docs/lambofile.md](docs/lambofile.md).

## How it is built

Strictly layered: **all** business logic lives in `lambo-core`; the CLI is a
thin shell that parses, renders and delegates. A future GUI consumes the same
core API and therefore cannot behave differently.

```
crates/lambo-core   the engine: config, detection, runtimes, services, doctor
crates/lambo-cli    the `lambo` binary (no logic of its own)
docs/               architecture, ADRs, references, guides
packaging/          the Windows installer definition
scripts/            install, package and installer automation
web/                the landing page (GitHub Pages)
```

Deep dive: [docs/architecture.md](docs/architecture.md) and the [ADRs](docs/adr/).

## Design principles

- **Never claim success you did not observe.** A service is "up" when its port
  answers, not when a process started.
- **Fail closed.** No checksum, no execution. No shell, no `cmd /c`, no
  privilege escalation, and never kill a process Lambo does not own.
- **No sudo for the daily workflow.** Unprivileged port 8080, per-user
  installs, everything under one directory.
- **Windows is designed for, not ported to.** Paths, processes, signals and
  shutdown are modelled through one `Os` abstraction that tests exercise on
  every platform.
- **Files you can read.** YAML configuration, plain-text logs, a state file
  you can inspect.

## Status

The engine and the CLI are complete and tested: **342 tests** - 299 core unit
tests, 37 integration tests against the public API, 6 CLI argument tests - with
a clean `cargo clippy --all-targets -D warnings` and a clean cross-compile to
`x86_64-pc-windows-msvc`. What is *not* done yet is the release engineering
around it - the catalogue needs pinned checksums before a fresh machine can
install PHP, and the macOS Apache/MariaDB entries do not exist. Nothing in the
Windows-specific code paths has been executed on physical Windows; that smoke
test is written out in [docs/windows.md](docs/windows.md). Tracked in
[docs/roadmap.md](docs/roadmap.md).

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/development.md](docs/development.md). Every layer has a documented
contract, every feature has tests, and every architectural decision has an ADR.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option. Contributions are licensed the
same way.
