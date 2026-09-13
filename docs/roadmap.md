# Roadmap

Where Lambo is, and what is deliberately not done yet.

## Shipped

**Engine, CLI and a native GUI** - the engine and CLI are complete and tested;
the GUI compiles and type-checks for Windows but has not been run.

- Localhost-first: ports default to 80/443 so the URL is `http://localhost`,
  with an automatic, explained fallback when the port cannot be bound
  (ADR-0007). The database manager is mounted at `/phpmyadmin` on the project's
  own port rather than given a port of its own.
- Application API (`lambo_core::app`): structured, serializable models that a
  GUI consumes directly. No front end parses another's output.
- GUI (`lambo-gui`): a Win32 dashboard over that API. **Cross-compiled and
  type-checked for `x86_64-pc-windows-msvc`, never executed** - see below.

- PHP: detect, download, verify, extract, per-version install, activate,
  generated `php.ini`, run without touching `PATH`
- Apache: install where a build exists, generated `httpd.conf`, config
  validation, graceful shutdown, HTTP health check
- MariaDB / MySQL: install, data directory, initialization, generated
  `my.cnf` bound to loopback, generated root password, TCP health, create /
  drop / list / shell
- Database UI: phpMyAdmin on loopback, `lambo db open`
- Projects: evidence-based `lambo init`, validated `lambo.yml`, non-destructive
  `.env`
- Orchestration: `lambo up / down / restart / status / open / logs`, with the
  full `up → HTTP 200 → status → down → no listener → no orphan` lifecycle
  verified end to end against a real child process and a real TCP connection
  (see [What "verified" means](#what-verified-means))
- Diagnostics: `lambo doctor`, every finding carrying its fix, with an
  `UNSUPPORTED` verdict for platform limitations so a missing macOS Apache
  build is reported as a fact with a workaround rather than as a failure
- Runtime management: `lambo php list` as a merged table of installed,
  available, active and platform-unavailable versions; a resolved `Runtime`
  view (executable, `php.ini`, extensions directory) that refuses to exist for
  an install with no executable; a verified download cache that recovers from
  an interrupted install
- Runtime execution: an install is not reported as successful until the binary
  has been started once and reported back. `RuntimeHealth` records the version
  PHP itself printed, the `php.ini` it says it loaded and its module list, so
  `lambo php current` and `lambo doctor` distinguish "installed" from "runs"
  and quote PHP's own error when it does not
- Process supervision: every service records its identity (boot-relative start
  time plus image name) and `lambo down` re-checks it before signalling, so a
  PID the operating system has reused is reported as stale instead of being
  terminated
- Migration: one-way `lambo migrate` from king-PHP
- Platform: Windows-first process, path and shutdown modelling
- Distribution readiness: artifact sources (official catalogue, explicit
  override, configured mirror, local artifact directory) resolved in one place
  with a fixed precedence, where the expected digest always comes from the
  catalogue and never from the source supplying the bytes; provenance recorded
  in every install manifest; `lambo config hash` for computing a digest without
  recording it; `lambo doctor --release` as the catalogue release gate
- Packaging: per-user Windows installer, portable zips, `SHA256SUMS`
- CI: test / clippy / fmt on Windows, Linux and macOS, plus cross-compile
  checks in both directions

Test suite: **488 tests** - 335 core unit tests, 37 written against the public
API the way a GUI would consume it, 26 over artifact source resolution and the
release gate, 13 that install a runtime which actually executes and assert on
what it reported, 10 lifecycle tests that drive the real orchestration against a
live server, 10 that drive the real download pipeline against committed
artifacts, 9 over the application API a GUI binds to, 18 over the GUI's view
layer, 7 Windows path tests, 6 that drive `lambo php` through the real install
pipeline, 6 CLI argument tests, 5 that drive the `lambo` binary through a full
service lifecycle, 1 doc-test, and 1 real-artifact test that CI skips - with
`cargo clippy --all-targets -D warnings` clean and a clean cross-compile to
`x86_64-pc-windows-msvc`.

### What "verified" means

Two different things are tested here, and the difference matters.

**Verified by automated tests.** Lambo's own behaviour: that `lambo up` binds a
port, that a real HTTP request against it returns 200, that `lambo status`
reports the service as running only while it answers, that `lambo down` stops
the process and releases the port, that no orphan survives, that a failed start
leaves nothing falsely reported as running, and that `lambo down` never touches
a process Lambo did not start. These run in CI on every platform.

Because real PHP, Apache and MariaDB artifacts cannot be downloaded in CI, the
server in those tests is a small fixture that accepts the same arguments as
`php -S` and is installed as the project's PHP runtime. Everything around it -
spawning, health checking, state, shutdown, the CLI, its exit codes - is the
production code path. The fixture is the only stand-in.

The runtime tests use a second fixture that actually executes: a `php` that
implements the command surface Lambo drives (`-v`, `--ini`, `-m`, `-r`, `-S`,
script paths) and refuses everything else loudly. So `PHPRC` wiring, health
checking, script execution and the post-install "does it run" gate are all
exercised end to end. A second archive verifies against its digest and then
refuses to start, which is precisely the case that gate exists for.

**Not verified automatically, and not implied by any of the above.** That real
PHP agrees with the fixture's output formats, that real Apache serves PHP
through `mod_php`, that real MariaDB initializes and accepts connections, and
that a real desktop browser opens. A green fixture run proves Lambo's plumbing;
it is not verification against a real PHP build, and the test output never
claims otherwise. `real_artifacts.rs` closes that gap when a developer stages a
genuine archive - it asserts that real PHP reports the declared version, loads
Lambo's generated `php.ini` and lists its built-in modules - and otherwise says
so and skips. The rest needs the pinned checksums in
[Next](#next-in-priority-order) and a physical machine; see
[windows.md](windows.md) for the manual smoke test.

## Next, in priority order

### 1. Pinned checksums for every catalogue entry

The single largest gap, and the only thing standing between the installer and
being usable out of the box. The catalogue ships with `sha256: null` for all 20
entries, so **a fresh machine cannot install anything until the user pins a
digest** - the downloader fails closed, correctly, but out of the box that means
"does not work yet".

The *mechanism* is finished and tested: `lambo doctor --release` reports exactly
which entries block a release, `lambo config hash` computes a digest, and a
maintainer records it in `catalogs/default.json`. What remains is the work that
needs network access to the upstream hosts, which this development environment
does not have.

Work: for each of the 20 entries, download the artifact, check its digest
against the publisher's own published value, and record it. Then add a CI job
that verifies every pinned digest still matches upstream.

Acceptance: `lambo doctor --release` exits 0, and on a clean Windows machine
`lambo php install 8.4` succeeds with no user action.

### 2. Apache and MariaDB for macOS

No catalogue entries exist. Until then macOS users install with Homebrew (which
Lambo will find on `PATH`) or use `server.kind: php`.

### 3. phpMyAdmin checksum

`lambo db install-ui` fails closed for the same reason as #1.

### 4. HTTPS

`server.https_port` is reserved and the configuration models it, but Lambo does
not yet generate a local CA or serve TLS. Until it does, `lambo up` serves
plain HTTP on 8080 and says so.

### 5. Multiple projects at once

Today one project owns the ports. Per-project ports already work; what is
missing is starting a whole `lambo workspace` and keeping a table of which
project holds which port.

### 6. Composer

Detected as evidence by `lambo init`, and given a directory under the home, but
not installed or managed. `lambo composer install` is deliberately *not* a
command: Composer is the user's own tool, and inventing a subcommand that
wraps it would promise more than it delivers.

## Not planned

These are conscious exclusions, not oversights.

- **Docker or container backends.** The premise is native execution.
- **A complicated GUI.** The engine is GUI-ready; a first GUI would be a thin
  rendering layer, and building a rich one now would compete with the engine.
- **DNS / `.test` domain resolution and `/etc/hosts` editing.** Requires
  elevation on every platform, which the default workflow must not need.
  `http://localhost` is the supported answer.
- **A plugin API.** Premature; the module boundaries would have to freeze
  first.
- **Windows services.** Lambo supervises processes itself. Registering a
  service needs elevation and outlives the session that started it, which is
  precisely what `lambo down` promises not to do.

## How to read a milestone claim

Anything in "Shipped" is covered by the test suite and by the manual Windows
smoke test in [windows.md](windows.md). Anything in "Next" is not implemented;
there is no stub that pretends otherwise - an unimplemented command does not
exist in the CLI rather than exiting successfully.
