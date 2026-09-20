# Roadmap

Where Lambo is, and what is deliberately not done yet.

## Shipped

**Engine, CLI and a native GUI** - all three are complete, and the engine is
audited symbol by symbol against the application it replaces. The window
itself has still never been
*run*: it compiles, its logic is tested on every platform, and it is
cross-compiled for Windows, but no one has watched it paint a pixel. That is the
one claim this roadmap will not make until the manual checklist in
[windows.md](windows.md) has been run on a physical machine.

- Localhost-first: ports default to 80/443 so the URL is `http://localhost`,
  with an automatic, explained fallback when the port cannot be bound
  (ADR-0007). The database manager is mounted at `/phpmyadmin` on the project's
  own port rather than given a port of its own.
- The panel: thirty services with their own cards, version menus, console and
  configuration buttons, plus the Projects, Editor, Vhosts and Settings pages,
  the tray icon, autostart and the log panel. Every page is described by
  `lambo_core::ui_state` and drawn by `lambo-gui`; the window holds no business
  logic, which is what keeps it identical to the CLI in behaviour.
- GUI (`lambo-gui`): a Win32 shell over the engine - the panel's own view and
  state are pure and tested (they run under `cargo test` on Linux, macOS and
  Windows), and `win32.rs` is drawing and event wiring over them.
  **Cross-compiled and type-checked for `x86_64-pc-windows-msvc`, never
  executed** - see below and the manual checklist the Windows page carries.
- The previous implementation's installation upgrades in place: its state file
  loads unchanged, its managed blocks are replaced rather than duplicated, and
  its downloaded archives are validated and adopted instead of re-fetched.

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

Test suite: **908 tests**, plus **477** more run by the local rustc harness on
machines where the crate graph is not available (which is where the framework
runner and the pure-logic modules are covered). The 908 are the engine's unit
tests (744), 37 written against the public API the way the GUI consumes it, 28
over the GUI's view layer, 26 over artifact source resolution and the release
gate, 16 lifecycle tests that drive the real orchestration against a live
server, 13 that install a runtime which actually executes and assert on what it
reported, 10 that drive the real download pipeline against committed artifacts,
10 over the GUI's panel state and the actions its controls raise, 7 Windows path
tests, 6 that drive `lambo php` through the real install pipeline, 6 CLI argument
tests, 4 over the upgrade path from the previous implementation's installation,
and 1 real-artifact test that CI skips - with `cargo clippy --all-targets
-D warnings` clean on the host and for `x86_64-pc-windows-msvc`, and a clean
cross-compile to that target.

The counts in this paragraph are the ones the closing run recorded.

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

**Mostly done.** The shipped catalogue pins a SHA-256 for every entry whose
publisher exposes a usable digest: PHP 8.4.2 on Windows, the Apache Lounge
httpd build, both MariaDB Windows zips and the Linux x86_64 tarball, and
phpMyAdmin on every platform - each pinned from the publisher's own checksum
document. Entries left at `null` verify through the upstream `<url>.sha256`
sidecar rule where the host publishes one (the older Windows PHP archives),
and fail closed where none exists: the static-php.dev PHP builds for Linux and
macOS, Oracle MySQL and Adminer publish no digest Lambo can consume. The two
Apache entries that pointed at apache.org directories that no longer carry
Windows binaries were removed; Apache is an Apache Lounge build now, the same
source the panel's resolver uses.

What remains: for each entry still at `null`, obtain a digest from its
publisher (or a source the project is willing to trust) and record it, then
add a CI job that verifies every pinned digest still matches upstream.

**The panel's own catalogue is a separate matter.** The thirty services the
dashboard offers are downloaded the way the application they were ported from
downloaded them: the URL and file name come from the catalogue, and the result
is validated by inspecting it (an empty file, a truncated archive or a saved
HTML error page is rejected and fetched again). Pinning those digests too would
be an improvement, but it is a *change* from the behaviour being preserved, so
it belongs here rather than in the port - and it is the same work as above: 29
more artifacts, downloaded once on a machine with network access.

Acceptance: `lambo doctor --release` exits 0, and on a clean Windows machine
`lambo php install 8.4` succeeds with no user action.

### 2. Apache and MariaDB for macOS

No catalogue entries exist. Until then macOS users install with Homebrew (which
Lambo will find on `PATH`) or use `server.kind: php`.

### 3. phpMyAdmin checksum

**Done.** phpMyAdmin's digest is pinned in the shipped catalogue, so
`lambo db install-ui` works out of the box. (Adminer's remains part of #1.)

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
- **A DNS server for `.test` domains.** Virtual hosts and the hosts file *are*
  supported (`lambo vhosts`, the Vhosts page) - writing the hosts file needs
  elevation, and Lambo says so instead of failing quietly - but running a
  resolver on port 53 would need a service, and the default workflow must not
  require either. `http://localhost` remains the answer that always works.
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
