# Architecture

This document is the map of the codebase. It states the rules that make Lambo
maintainable for a decade - and every PR is reviewed against them.

## The one rule that shapes everything

**All business logic lives in `lambo-core`. Interfaces are thin shells.**

```
┌──────────────────┐        ┌──────────────┐
│       GUI        │        │  CLI (lambo) │   parse → delegate → render
│   (future)       │        │    clap      │   ZERO business logic
└────────┬─────────┘        └──────┬───────┘
         └────────────┬────────────┘
                      ▼
        ┌───────────────────────────────┐
        │         CORE ENGINE           │  pure logic, no UI dependency,
        │  config · lambofile · detect  │  no global state, unit-tested
        │  runtime · catalog · download │
        │  php · apache · database ·    │
        │  dbui · session · doctor      │
        └───────────────┬───────────────┘
                        ▼
        ┌───────────────────────────────┐
        │   PROCESS · PORT · HTTP · FS  │  the platform seam: one `Os`
        │      modelled through `Os`    │  argument, never a cfg switch
        └───────────────┬───────────────┘
                        ▼
        ┌───────────────────────────────┐
        │  Windows · Linux · macOS      │  native - no containers
        └───────────────────────────────┘
```

Why this pays off:

- **Testing is trivial.** Behaviour is testable without a terminal, a display
  server, or interactive input.
- **Interfaces converge by construction.** A GUI cannot accept a configuration
  value the CLI rejects - both call `Config::set`.
- **Windows is testable on Linux.** Because `Os` is an argument, the test suite
  exercises Windows path shapes, executable resolution and command lines on
  every platform.

## Crate map

| Crate | Role | Dependencies |
| --- | --- | --- |
| `lambo-core` | The engine: all domain logic | serde, serde_yaml, serde_json, semver, thiserror, flate2, crc32fast |
| `lambo-cli` | The `lambo` binary: parse, render, delegate | clap, lambo-core, thiserror |

Dependency rule: interface crates depend on `lambo-core`; `lambo-core` depends
on **nothing internal**. Any arrow in the other direction is a design bug.

## `lambo-core` module map

Modules map 1:1 to concepts a user can name.

| Module | Responsibility |
| --- | --- |
| `platform` | The `Os`/`Arch`/`Platform` model: executable names, search directories, path defaults |
| `paths` | The home directory and every path under it; `LAMBO_HOME` |
| `config` | Global configuration, key validation, credential generation |
| `lambofile` | The project file: parse, validate, serialize, ancestor search |
| `detect` | Evidence-based framework detection; every probe is reported |
| `naming` | Slugifying, database names, local URLs |
| `version` | Version specs (`stable`, `~8.3`, `^8.2`, `8.4.2`) |
| `catalog` | The embedded download catalogue plus user overrides, and `validate()` - every entry checked before it can be used |
| `download` | HTTPS-only downloads, checksum resolution **before** fetching; returns the digest actually verified against |
| `sha256` / `archive` | Digests; ZIP and tar.gz extraction with traversal and size limits |
| `runtime` | Installed runtimes, the `.active` marker, executable discovery, install manifests and the integrity check behind `corrupt` |
| `php` | PHP install/activate/remove, `php.ini` generation, `php -S`, and `RuntimeHealth` - starting the binary to confirm it actually runs |
| `apache` | Generated `httpd.conf`, validation, graceful shutdown |
| `database` | MariaDB/MySQL: config, init, security, CRUD, shell |
| `dbui` | phpMyAdmin (or Adminer) served on loopback, aliased at `/phpmyadmin` |
| `browser` | Opening a URL with no shell on any platform |
| `envfile` | Non-destructive `.env` editing |
| `process` | Spawn, stop, tree termination, liveness - no shell anywhere |
| `port` | Free/occupied checks, alternatives, occupant identification |
| `http` | Loopback health checks |
| `state` | What Lambo started, in start order |
| `logs` | Log locations, tail, follow, clear |
| `session` | The ordered `up`/`down`/`status` orchestration |
| `doctor` | Diagnostics; every finding carries its fix |
| `project` | The loaded project: resolved settings, validation, `.env` keys |
| `migration` | The one-way king-PHP → Lambo upgrade |
| `secret` | Password generation from in-tree entropy |
| `fsx` | Atomic writes, owner-only permissions |
| `yaml` | The crate-private YAML facade (ADR-0004) |
| `workspace` | Named groups of projects |

## The platform seam

This is the part most worth understanding, because it is where "Windows
support" either exists or does not.

`Os` is a value, not a compile-time condition:

```rust
pub fn is_executable_file(path: &Path, os: Os) -> bool
pub fn command_spec(plan: &Plan, config_file: &Path, log: &Path) -> ProcessSpec
pub fn initialize_spec(database: &Database, plan: &Plan, os: Os) -> Option<ProcessSpec>
```

Consequences, all deliberate:

- A test on Linux can assert the exact Windows command line, including
  `.exe` suffixes and forward-slashed config paths.
- Production code contains almost no `#[cfg(windows)]`. The exceptions are the
  two places the OS itself differs irreducibly: `CREATE_NEW_PROCESS_GROUP` when
  spawning, and the executable-bit check, which has no Windows equivalent.
- Nothing anywhere shells out. No `cmd /c`, no `/bin/sh`, no string
  interpolation into a command line. Every process is a program plus an
  argument vector, so an argument containing a space or a quote cannot turn
  into a second command.

### Stopping things

`terminate_tree` signals the recorded pid, and on Windows uses
`taskkill /T`, which the OS scopes to that process's tree. It does **not**
signal a process group derived from a pid: a group id read from a state file
may have been recycled, and signalling it can hit processes Lambo has never
seen. The cost is that a Unix child which double-forks may survive; the
benefit is that Lambo can never kill something it does not own.

## Data flow of `lambo up`

```
Project::load            lambo.yml + detection, validated
      │
session::up
      ├─ validate          fail before anything starts
      ├─ credentials       generate once, store in the global config
      ├─ preflight         every port checked before anything binds
      ├─ ensure_php        resolve, else install (verified)
      ├─ start_database    init → start → TCP health → secure → create db
      ├─ write_env         .env keys, never overwriting a set value
      ├─ start_server      generate config → validate → start → HTTP health
      ├─ state.save        recorded after every start, not at the end
      └─ open browser      only once the URL answers
```

Two properties are load-bearing:

- **State is saved after each service starts**, not at the end. A failure at
  step 6 leaves steps 1–5 recorded, so `lambo down` can clean up.
- **Nothing is reported as up until it was observed.** A started process that
  does not answer its port is a failure with the log path attached.

## Error model

`Error` carries context, not just a message. `ServiceFailed` holds the
service, the reason, a list of causes and a hint; `details()` renders them for
the CLI. A user reading only the first line learns what broke; a user reading
the rest learns what to run next.

Distinct situations get distinct variants rather than distinct prose, because a
caller that needs to react must be able to match. `VerificationUnavailable`
carries the URL, family, version, platform and remedy as fields - it is not a
`Download` failure, since nothing was fetched and no digest was wrong. The
database-manager install path used to recover its own hint by matching the
substring `checksum is unavailable` inside a formatted message; it now matches
the variant.

## Distribution integrity

Three checks stand between an artifact on the network and a runtime on disk, and
they are ordered so the cheapest runs first:

1. **The catalogue is valid.** `Catalog::validate()` rejects an entry whose
   format Lambo cannot unpack, whose platform key it does not know, or whose
   digest is malformed - before any network call.
2. **The bytes are what they claim to be.** The digest is resolved *before* the
   download: from the entry, or from a `.sha256` published beside it. With
   neither, the install is refused and nothing is fetched.
3. **What was installed is recorded.** The install writes a manifest into the
   runtime naming the version, platform, source, digest and file list. Later
   checks compare the directory against it, which is how `lambo php list`
   distinguishes a working runtime from one that is missing half its
   extensions.

An install is transactional at the last step: the previous runtime is moved
aside rather than deleted, and restored if promoting the new one fails. A failed
upgrade leaves the user with the version they started with.

### Where the bytes come from

`lambo_core::sources` is the single place that decides, in a fixed precedence:
an explicit catalogue override, a local artifact found by its identity-derived
name, a configured mirror, then the official URL.

The rule that makes this safe is that the *bytes* may come from anywhere on that
list but the *expected digest* may only come from trusted metadata - the digest
pinned in the catalogue, or the `.sha256` sidecar published beside the
**official** URL. A mirror never supplies the digest for the bytes it serves,
and neither does a directory of local files. Letting the source vouch for its own
contents is not verification, it is a formality.

So a mirror that serves different bytes fails with a checksum mismatch, and a
local file with the right name and the wrong contents fails the same way. There
is no `--skip-checksum` and no environment variable that disables this: such a
flag would not weaken a check, it would remove the only reason to trust what got
installed.

Each install records its provenance - `catalogue`, `override`, `mirror`, `local`,
or `unknown` for a manifest written before source tracking existed. `unknown` is
reported as unknown rather than defaulted to `catalogue`, because claiming an
install came from the official host when the manifest does not say so sends
somebody debugging a mirror problem somewhere useless.

### Two grades of validation

`Catalog::validate()` is what a running Lambo needs: a missing digest is a fact
about the world, reported and failed closed. `Catalog::validate_release()` is the
release gate, where a missing digest is an error, because a published catalogue
whose entries cannot be installed is not shippable. `lambo doctor --release`
runs the latter; it validates metadata and downloads nothing.

## Testing

397 tests, in layers that each prove something the layer below cannot:

| Layer | Count | What it proves |
| --- | --- | --- |
| `lambo-core` unit tests | 322 | Parsing, validation, path shapes, generated configuration, archive safety, port logic, process supervision, the session state machine |
| `public_api.rs` | 37 | The API is usable from outside the crate, the way a GUI would consume it |
| `lifecycle.rs` | 10 | `up → HTTP 200 → status → down → no listener → no orphan`, against a real child process |
| `runtime_distribution.rs` | 10 | The download pipeline against committed artifacts with real digests |
| `windows_paths.rs` | 7 | Windows path shapes, asserted while running on Linux |
| `cli_lifecycle.rs` | 5 | The real `lambo` binary, driven through `init/up/status/down` |
| CLI argument tests | 6 | Argument parsing |

Test doubles are real, not mocks. `LocalDownloader` serves `file://` fixtures
through the same code path as `curl`, so archive extraction and checksum
verification are genuinely exercised. The lifecycle fixture is a real child
process reached over a real TCP socket; a test that asserted only "a process was
spawned" would pass just as happily against a server that never bound a port,
which is precisely the failure Lambo must never report as success.

Because real PHP, Apache and MariaDB artifacts cannot be downloaded in CI, the
lifecycle fixture stands in for `php -S`. Everything around it is the production
code path. What that leaves unverified - real `mod_php`, real MariaDB
initialization, a real desktop browser - is a manual smoke test; see
[windows.md](windows.md) and the roadmap's
[What "verified" means](roadmap.md#what-verified-means).

The runtime fixtures go one step further than a mock: `tests/fixtures/runtime/`
contains a `php` that actually executes, implementing the command surface Lambo
drives (`-v`, `--ini`, `-m`, `-r`, `-S`, script paths) and refusing everything
else loudly. So `PHPRC` wiring, health checking, script execution and the
post-install "does it run" gate are exercised end to end, not asserted about. A
second fixture archive verifies against its digest and then refuses to start,
which is the case the gate exists for. See [runtime.md](runtime.md#what-the-automated-tests-prove)
for exactly what this does and does not prove - a green fixture run is not
verification against a real PHP build, and that distinction is preserved in the
test output rather than glossed over.

A test that would signal a process group does not exist, because on some hosts
that kills the test runner itself - and because a group signal is the wrong tool
in production too: see `process::terminate_tree`.

`cargo clippy --workspace --all-targets -- -D warnings` is clean, as is
`cargo check --target x86_64-pc-windows-msvc --all-targets`.
