# Architecture

This document is the map of the codebase. It states the rules that make Lambo
maintainable for a decade - and every PR is reviewed against them.

## The one rule that shapes everything

**All business logic lives in `lambo-core`. Interfaces are thin shells.**

```
┌──────────────────┐        ┌──────────────┐
│       GUI        │        │  CLI (lambo) │   parse → delegate → render
│  Windows, native │        │    clap      │   ZERO business logic
│  panel (primary) │        │  (secondary) │
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
| `lambo-gui` | The native Windows panel: cards, tables, editor, tray, and the Win32 seam | lambo-core, windows-sys |
| `lambo-process-windows` | The Windows process primitives (job objects, tree termination) | windows-sys |

Dependency rule: interface crates depend on `lambo-core`; `lambo-core` depends
on **nothing internal**. Any arrow in the other direction is a design bug.

### How the panel keeps that rule

`lambo-gui` is four modules, ordered by how much of it can be checked without a
Windows desktop:

| module | role | Win32? |
| --- | --- | --- |
| `view.rs` | how it looks: palette, fonts, DPI, identifier conversions | no |
| `state.rs` | the panel's own state, and what each control asks for (`Action`) | no |
| `ops.rs` | the engine call behind each action | no |
| `win32.rs` | controls, painting, messages, the tray | yes |

The first two are compiled and tested by the local rustc harness (its `gui_view`
and `gui_state` modules), because a decision about a colour, a layout or a
control's meaning is not a Win32 call. The third and fourth are type-checked by
CI against the real `windows-sys`; until CI has said so, the only claim made for
them is that they have been compiled for `x86_64-pc-windows-msvc` by a
Windows-target compiler with `windows-sys` 0.59, which is not the same thing;
the verification round recorded it as such. The rule is visible in what is *absent*: there
is no second copy of a service's status in the panel - `stack::ManagedService`
answers that - no vhost validation, no project naming rule, and no download
logic. `ui_state` (in `lambo-core`, not in the GUI) owns the layout and the
widget list, so the window has nothing to compute and the CLI could use the same
geometry if a terminal front end ever wanted to. The same rule holds for the two
menus: the tray's lines are `lambo-core::tray::menu`, and a card's version menu -
its title, its variants and which of them carries the check mark - is
`ui_state::{version_menu, version_menu_title}`. What is left in the window is the
popup, the position it opens at and the id it reports back.

## `lambo-core` module map

Modules map 1:1 to concepts a user can name.

| Module | Responsibility |
| --- | --- |
| `platform` | The `Os`/`Arch`/`Platform` model: executable names, search directories, path defaults |
| `paths` | The home directory and every path under it; `LAMBO_HOME` |
| `config` | Global configuration, key validation, credential generation |
| `lambofile` | The project file: parse, validate, serialize, ancestor search |
| `panel` | The control panel's installation state (`config.json`): services, virtual hosts, projects, settings, and the migration rules that carry an older file forward |
| `detect` | Evidence-based framework detection; every probe is reported |
| `naming` | Slugifying, database names, local URLs |
| `version` | Version specs (`stable`, `~8.3`, `^8.2`, `8.4.2`) |
| `catalog` | The embedded download catalogue plus user overrides, and `validate()` - every entry checked before it can be used |
| `catalog_panel` | The previous implementation's catalogue as data: the 29 installable components with their versions, download coordinates, kinds, per-version variants, the two live resolvers and the post-install hooks, and the `InstallPlan` each one resolves to |
| `download` | HTTPS-only downloads, checksum resolution **before** fetching; returns the digest actually verified against |
| `download_cache` | The shared `downloads/` cache: files are adopted as verified when this process writes them, anything older is validated before use, and the startup sweep collects provably-broken entries |
| `vendor` | The two downloads with no fixed URL (Apache Lounge's newest Win64 build, Zig's newest stable release) and the index parsing that picks them |
| `installer` | The install workflow the panel drives: cache lookup, download, unpacking, silent installers, hook dispatch, activation (a versioned install mirrored into the canonical directory) and the PATH-refresh seam |
| `postinstall` | Everything after an archive is unpacked: the generated `httpd.conf` patch (installation root, document root, PHP handler block, vhost include), the shipped welcome page and runtime DLLs, `php.ini`, MariaDB and PostgreSQL initialisation, phpMyAdmin/Adminer, Composer, pip, rustup, RabbitMQ and MinIO state - each reached through `run(hook, ..)` |
| `vhost` | Virtual hosts: the hosts file, the Apache vhost include and one nginx site file per host, each written into a managed block that is replaced rather than appended to - plus the virtual-hosts page's rules (the form's validation, the row's five cells, the extension selection, the enabled flag an edit keeps, the project domain move), so the page and `lambo vhosts` share one implementation |
| `sha256` / `archive` | Digests; ZIP and tar.gz extraction with traversal and size limits |
| `runtime` | Installed runtimes, the `.active` marker, executable discovery, install manifests and the integrity check behind `corrupt` |
| `php` | PHP install/activate/remove, `php.ini` generation, `php -S`, and `RuntimeHealth` - starting the binary to confirm it actually runs |
| `apache` | Generated `httpd.conf`, validation, and the command line the engine runs |
| `database` | MariaDB/MySQL: config, init, security, CRUD, shell |
| `dbui` | phpMyAdmin (or Adminer) served on loopback, aliased at `/phpmyadmin` |
| `browser` | Opening a URL or a folder with no shell on any platform |
| `envfile` | Non-destructive `.env` editing |
| `process` | Spawn, stop, tree termination, liveness - no shell anywhere |
| `port` | Free/occupied checks, alternatives, occupant identification |
| `service` | One supervised process, as `service.go` runs it: the start preconditions, the port check, PostgreSQL's `runas`/`--hide-run` launch and its adopted `postmaster.pid`, the log streaming, the exit reporting, and the stop order (`pg_ctl` first, then the tree) - over the `ServiceHost` seam the tests script |
| `zombies` | The startup sweep: the process table is read through the host, `\`-normalised and case-folded, and everything whose executable sits under `<base>/bin/` is killed with the message the log shows |
| `console` | The two consoles the panel opens: a language runtime's terminal (the original's `langBinDirs`, ConEmu preference, `PATH` and `LAMBO_BASE`) and PostgreSQL's, both as argument vectors the Windows interface runs with a console of their own |
| `stack` | The installation's services as one stack: a `ManagedService` per entry of the panel configuration, the card's start decision table (install-before-start, `exe missing`, open URL), the two essential passes, Stop All, `settings.auto_start`, the startup sweep, the toolbar's restart (stop, sweep, pause, essentials) and the web-server picker's switch (stop every other running web server) |
| `pathenv` | The user `PATH`: the directories that belong on it, the collision rules, and the registry seam behind them |
| `tray` | The tray menu model and the "start with Windows" setting: the `Run` value names, the command line, the check mark, and the store seam behind them |
| `http` | Loopback health checks |
| `logs` | Log locations, tail, follow, clear, and the `LogFn` sink the engine narrates through |
| `session` | The ordered `up`/`down`/`status` orchestration: it generates the configurations a service reads, points the panel configuration at them, and hands the run to `stack`. The one process it starts itself is a project's own web server - `server.kind: php`, where the runtime's `php -S` is supervised by the same `service` engine (`session::project_server`) and reported as `php-server`. It also holds the entry points the two interfaces call for projects and virtual hosts (`create_project`, `delete_project`, `set_project_domain`, `vhosts`, `save_vhost`, `delete_vhost`, `apply_vhosts`) over `PanelBook`, the installation document - which is also what records a version switch (`set_active_version`), so the build a card's picker chose survives a restart |
| `doctor` | Diagnostics; every finding carries its fix |
| `project` | The loaded project: resolved settings, validation, `.env` keys |
| `migration` | The one-way king-PHP → Lambo upgrade |
| `secret` | Password generation from in-tree entropy |
| `frameworks` | The framework catalogue and the project scaffolder: 17 scaffolders, tool resolution, Composer, the four scaffold strategies, and the registration of a project and its vhost in the panel document |
| `fsx` | Atomic writes, owner-only permissions |
| `yaml` | The crate-private YAML facade (ADR-0004) |
| `serde_defaults` | The one deserialization rule the state files need: a `null` list is an empty list. `#[serde(default)]` covers a missing key, not a key whose value is `null` - and the previous implementation writes `"projects": null`, so without this its own configuration file would not open |
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

`terminate_tree` signals the process the engine holds, and on Windows uses
`taskkill /T`, which the OS scopes to that process's tree. It does **not**
signal a process group derived from a pid: a group id computed from a pid may
have been recycled, and signalling it can hit processes Lambo has never
seen. The cost is that a Unix child which double-forks may survive; the
benefit is that Lambo can never kill something it does not own.

## Data flow of `lambo up`

```
Project::load            lambo.yml + detection, validated
      │
session::up
      ├─ validate          fail before anything starts
      ├─ prepare           PHP runtime ensured, then the configurations the
      │                    services read: httpd.conf (written, then checked by
      │                    `httpd -t`) and my.cnf; the panel configuration's
      │                    entries are pointed at them (`-f`, `--defaults-file`).
      │                    Only what the project asks for: `database.kind: none`
      │                    writes no database configuration at all
      ├─ serve             one of two branches
      │                    `server.kind: php` → the project's own server: the
      │                    engine runs `php -S 127.0.0.1:<port> -t <docroot>`
      │                    and the pass waits for that URL
      │                    anything else     → Stack::ensure_essentials over the
      │                    active web server, PHP-FPM, the database and the
      │                    manager (installing what the catalogue says is
      │                    missing), then the project's URL is waited for
      └─ open browser      the page the pass ends on
```

`up` is idempotent in both branches: a server that is already serving the
project's port is reported with the engine's own `already running (pid N)`
rather than started a second time, and it is recognised from another process -
which matters because `lambo up` and the `lambo status` that follows it are two
processes.

Two properties are load-bearing:

- **The engine holds what it started**, so a failure half-way through leaves
  everything that did start owned by a live `Service` - `lambo down` stops
  exactly those processes, and there is no file that can disagree with them. A
  process this engine did *not* hold is found by its executable or by the port
  it holds ([`Service::pid`]), so `status` and `down` answer for a server another
  Lambo process started; the startup sweep keeps only the pids this engine
  holds, because everything else under `<base>/bin/` is a leftover.
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

908 tests, in layers that each prove something the layer below cannot:

| Layer | Count | What it proves |
| --- | --- | --- |
| `lambo-core` unit tests | 744 | Parsing, validation, path shapes, generated configuration, archive safety, port logic, process supervision, the session state machine, the panel's state model and every action its controls raise |
| `public_api.rs` | 37 | The API is usable from outside the crate, the way a GUI would consume it |
| `lambo-gui` unit tests | 28 | `view.rs` and `state.rs`: the palette, geometry, button text, and the routing of every control to an engine call |
| `sources.rs` | 26 | Artifact source resolution and its precedence, and the release gate over the catalogue |
| `lifecycle.rs` | 16 | `up → HTTP 200 → status → down → no listener → no orphan`, against a real child process |
| `runtime_execution.rs` | 13 | A runtime that is installed, started, asked its version, and stopped - asserting on what it reported |
| `cli_lifecycle.rs` | 10 | The real `lambo` binary, driven through `init/up/status/down` |
| `runtime_distribution.rs` | 10 | The download pipeline against committed artifacts with real digests |
| `windows_paths.rs` | 7 | Windows path shapes, asserted while running on Linux |
| `cli_php.rs` | 6 | `lambo php` through the real install pipeline |
| CLI argument tests | 6 | Argument parsing |
| `legacy_installation.rs` | 4 | An installation left by the implementation Lambo replaces: its state file loads unchanged, its archives are adopted rather than re-fetched, its markers are replaced rather than duplicated |
| `real_artifacts.rs` | 1 | A genuine upstream archive, when one is staged; skipped loudly otherwise |

Three further binaries exist and contribute nothing on Linux by design:
`windows_handle_inheritance`, `lambo-process-windows`, and the fixture server
`tests/fixtures/fixture_server`. On Windows the first two run.

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
