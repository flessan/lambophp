# Lambo PHP

> **Release Candidate `0.14.0-rc.1` - not yet validated on a physical Windows
> machine.**
> A Windows build has been produced, and one report says that launching it on
> Windows 11 fails with
> `The application was unable to start correctly (0xc000001e)`. That code means
> Windows could not map the executable, and it is the one class of failure no
> amount of testing on other machines can see. `LamboPHP-Setup.exe` is in the
> assets of the release; the diagnosis steps are in
> [docs/windows.md](docs/windows.md#if-nothing-starts-at-all) and the acceptance
> checklist in the same file is what this release is waiting on. **Nothing in
> this repository claims the window has been run.**

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

[![CI](https://github.com/flessan/lambophp/actions/workflows/ci.yml/badge.svg)](https://github.com/flessan/lambophp/actions/workflows/ci.yml)
[![Release](https://github.com/flessan/lambophp/actions/workflows/release.yml/badge.svg)](https://github.com/flessan/lambophp/actions/workflows/release.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

📖 **[Docs](docs/)** · 🪟 **[Windows guide](docs/windows.md)** ·
🗺️ **[Roadmap](docs/roadmap.md)** ·
💬 **[Discussions](https://github.com/flessan/lambophp/discussions)**

---

## Install

**Windows 10/11** - download `LamboPHP-Setup.exe` from the
[latest release](https://github.com/flessan/lambophp/releases), or use the
portable `LamboPHP-windows-amd64.zip` and put `lambo.exe` anywhere you like.
No administrator rights are needed.

**Linux / macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/flessan/lambophp/main/scripts/install.sh | sh
```

Everything Lambo owns lives under one directory - `%USERPROFILE%\Lambo` on
Windows, `~/.lambo` elsewhere - and `LAMBO_HOME` moves it anywhere, including
onto a USB stick. Details: [docs/installation.md](docs/installation.md).

> [!IMPORTANT]
> **Every verifiable download is checksum-pinned; the rest fail closed.**
> The shipped catalogue pins a SHA-256 for every entry whose publisher exposes
> a usable digest - PHP 8.4.2 on Windows, Apache (Apache Lounge build),
> MariaDB and phpMyAdmin - and verifies the others against their upstream
> `.sha256` sidecars where those exist. Entries with neither (the static-php
> PHP builds for Linux/macOS, Oracle MySQL, Adminer) are refused until a
> digest is pinned. That is deliberate: a tool that executes downloaded
> binaries should not guess about their integrity. One file fixes it - see
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

Strictly layered: **all** business logic lives in `lambo-core`, and both
interfaces are thin shells that parse, render and delegate. There is one
engine, so they cannot disagree.

```
GUI ─┐
     ├──> crates/lambo-core
CLI ─┘

crates/lambo-core            the engine: config, state, detection, runtimes,
                             services, host and vhost files, doctor
crates/lambo-cli             the `lambo` binary (no logic of its own)
crates/lambo-gui             the Win32 panel (no logic of its own)
crates/lambo-process-windows the Win32 process backend the engine calls
docs/                        architecture, ADRs, references, guides
packaging/                   the Windows installer definition
scripts/                     install, package and installer automation
web/                         the landing page (GitHub Pages)
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

The engine, the CLI and the window are written, and the whole of the
application Lambo replaces was audited against them symbol by symbol: every
function, type, constant and variable mapped to what it became, with the four
things deliberately not carried over recorded in the same pass. Locally:
**908 tests passing** (and 477 more through the local harness), `cargo clippy
-D warnings` clean on the host and for `x86_64-pc-windows-msvc`, and a clean
cross-compile to that target.

The catalogue now pins a SHA-256 for every entry whose publisher exposes a
usable digest (PHP 8.4.2 on Windows, the Apache Lounge httpd build, MariaDB
and phpMyAdmin), verifies against upstream sidecars where those exist, and
fails closed for the entries no publisher digest covers - the remaining gap
there is data collection, listed in
[docs/roadmap.md](docs/roadmap.md#1-pinned-checksums-for-every-catalogue-entry).
The `windows-behaviour` CI job's test filter has been corrected, so its
83 previously-skipped tests run.

One thing is *not* done: no one has watched the window run. Every claim about
`win32.rs` in this repository is a claim about code that compiles and is
type-checked for Windows, never about pixels on a screen. The checklist that
closes that gap is [docs/windows.md](docs/windows.md); CI's verdict is not
readable from the environment this work was done in, and
[docs/release.md](docs/release.md) says exactly what that means.

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) and
[docs/development.md](docs/development.md). Every layer has a documented
contract, every feature has tests, and every architectural decision has an ADR.

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE) at your option. Contributions are licensed the
same way.
