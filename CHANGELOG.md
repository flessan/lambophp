# Changelog

All notable changes to Lambo PHP are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

_Nothing yet._

## [0.14.0-rc.1] - release candidate

This is a **release candidate**, not a stable release. It has not been
validated on a physical Windows machine; see
[docs/windows.md](docs/windows.md) for the acceptance checklist that still has
to be run by hand.

### Changed

- A server whose process exits during start-up is now reported immediately,
  with the exit code, the URL it was meant to serve, its log path and the
  command it was started with. Previously `lambo up` polled a dead port for the
  full 30-second HTTP bound before saying anything, which read as a hang in
  both the CLI and the GUI. A server that is alive but slow still gets the
  whole bound: only a *dead* process fails fast. Apache is deliberately
  excluded, because `httpd -f` daemonises on Unix and its recorded PID is a
  parent that exits normally.
- The Dashboard distinguishes a live process from a serving site. A stack that
  is up but not yet answering reads `Starting` rather than `Running`, so the
  window cannot send you to a URL that refuses the connection.
- Settings shows eleven curated rows with plain-language captions instead of
  every internal key. The database password is masked and never rendered as a
  value.
- About now states the licence and links the repository, both read from the
  build rather than typed into the GUI.

### Fixed

- A failure could be attributed to a name that is not a service - an error
  naming the runtime `"PHP stable"` was recorded under a key no service row can
  match, so nothing showed as `Failed` even though something had failed.
  Attribution now only blames a managed service name.
- `lambo --help` advertised `http://localhost:8080`; the server binds port 80.
- The NSIS installer no longer falls back to version `0.0.0`. A missing
  `/DVERSION` is now an error, so a mis-invoked build cannot ship an installer
  that misidentifies itself.
- The Windows package can no longer be built without its licence files, and the
  release workflow fails rather than publishing a ZIP with no installer.

### Added

- `docs/windows.md` carries the twenty-step manual acceptance checklist, every
  row marked `MANUAL WINDOWS TEST`.

## [0.12.3] - the window says "Starting" while it is starting

### Fixed

- Starting and stopping are synchronous and can take several seconds while a
  runtime downloads or a database initialises. Until now the window kept
  showing the previous state for that whole time, so "working" and "hung"
  looked identical and a user who clicked twice was not being told anything.
- The state line now reads **Starting…** or **Stopping…** for the duration, and
  both buttons are refused while an operation is in flight - a second Start
  would race the first, and Stop cannot interrupt a start.

With this the window communicates all four states the product promises:
Stopped, Starting, Running and Failed.

Verified: 484 tests pass, fmt clean, clippy `-D warnings` clean on the Linux
workspace and on `lambo-gui` for `x86_64-pc-windows-msvc`, and the Windows
target cross-checks with no errors.

NOT VERIFIED: the in-progress repaint has not been observed on physical
Windows - `NOT PERFORMED BY AGENT`. No `.exe` was produced here; `link.exe` is
absent from this sandbox.

## [0.12.2] - the GUI shows the remedy, not just the error

### Fixed

- A failed operation showed only its error string. `OperationOutcome` also
  carries the likely causes and the command to run next - the engine had
  already worked both out, and the window discarded them. A notice that says
  "port 80 is held by another process" and stops there sends the user to a
  terminal to find the fix Lambo already knew.
- `view::failure_message` now assembles what failed, what to try, and what
  Lambo knows into the notice. Causes are capped at two, because a notice line
  is not a scroll view.

Verified: 484 tests pass, fmt clean, clippy `-D warnings` clean on the Linux
workspace and on `lambo-gui` for `x86_64-pc-windows-msvc`, and the Windows
target cross-checks with no errors.

## [0.12.1] - the GUI can actually choose a project

### Fixed

- **`Start` could never work.** `Ui.project` was initialised to `None` and
  nothing anywhere in the crate ever assigned to it, so the Start button
  always answered "No project is open" and the required workflow - install,
  open Lambo, choose a project, start - stopped at the second step. The
  Projects screen listed projects and clicking one did nothing.
- Project rows on the Projects screen are now selectable: the status statics
  are created with `SS_NOTIFY` and a click selects that project. The registry
  is re-read at click time rather than cached, so a project added or removed
  since the window opened cannot be selected by a stale index.
- The window opens on the first registered project instead of on none, so a
  single-project install - the common case - is startable immediately. The
  Projects screen still switches.

Verified: 482 tests pass, fmt clean, clippy `-D warnings` clean on the Linux
workspace and on `lambo-gui` for `x86_64-pc-windows-msvc`, and the Windows
target cross-checks with no errors. The selection interaction itself runs only
on Windows: `NOT PERFORMED BY AGENT`.

## [0.12.0] - the GUI has all eight screens

Until now `lambo-gui` was a dashboard with an About dialog. The other screens
existed as data in `lambo_core::app` and nowhere else.

### Added

- **Projects, PHP, Services, Database, Logs and Settings screens**, navigable
  from a row across the top of the window. Each is rendered by the
  platform-independent `view` module from the same `lambo_core::app` calls the
  CLI makes, so the window and a terminal cannot disagree.
- **About became a screen** rather than a modal, so it is reached the same way
  as everything else.
- A screen that cannot read its data says so in the line area instead of
  showing a stale or empty one.

### Notable decisions

- **Settings lists every key the engine knows**, from `Config::KEYS`, rather
  than a hand-picked "interesting settings" list. A settings screen with a
  silent hole is worse than one that is long.
- **A runtime that will not run says why** on the PHP screen, instead of
  looking like one that merely is not selected.
- **`unsupported` is not a fault.** Doctor findings at that severity render as
  fine, because a feature being unavailable on this platform is not something
  the user did wrong.
- **Logs show the file path** and the tail oldest-first. The point of the
  screen is often "where do I look", and reading a log backwards is not
  reading it.
- **The Services screen lists the database manager**, which the dashboard
  deliberately omits because there it is a button.

Verified: 482 tests pass, fmt clean, clippy `-D warnings` clean on the Linux
workspace and on `lambo-gui` for `x86_64-pc-windows-msvc`, and the Windows
target cross-checks with no errors. The screens' layout and interaction have
**not** been exercised on physical Windows: `NOT PERFORMED BY AGENT`.

## [0.11.0] - a Windows release you can actually run

### Added

- **`scripts/build-windows.ps1`** - the one entry point for a Windows release.
  `-Version` (defaults to `Cargo.toml`), `-Arch amd64|arm64`, `-SkipTests`.
  Runs the suite with `--no-fail-fast`, builds the CLI and the GUI, writes
  `dist/LamboPHP-<version>-windows-<arch>/` + `.zip` + `LamboPHP-Setup.exe`,
  and emits a `.sha256` per artifact. It refuses to package when
  `lambo-gui.exe` is missing, and fails if a test fixture leaks into the
  payload. `release.yml` now calls it, so a CI release and a hand-run one
  produce the same files.
- **20 more steps in the Windows manual test plan**: the GUI (agreement with
  the CLI, About, `Failed` presentation, no Rust backtrace), upgrade
  preservation of projects/PHP/databases/config/logs, and hostile paths -
  spaces, non-ASCII, a substituted drive.
- Steps 17a/17b asserting `http://localhost/phpmyadmin` while the project is
  up, and the standalone fallback when it is not.

### Fixed

- **`ci.yml` did not parse.** Line 110 was `run: cargo test ... session::`,
  and a plain YAML scalar ending in a colon is a mapping indicator, so the
  whole workflow would have failed to load. Both workflows are now validated
  with a real YAML parser.
- **`docs/windows.md` documented ports the product does not use.** It claimed
  HTTP `8080` / HTTPS `8443`; the shipped defaults are `80` / `443`, which is
  the point of the localhost-first design. Step 23 tested a conflict on 8080,
  a port nothing binds by default any more.

## [0.10.0] - phpMyAdmin becomes the database UI

The product promised `http://localhost/phpmyadmin` while installing Adminer
behind it. This release makes the promise true.

### Changed

- **phpMyAdmin is the default database manager** (`dbui.kind = phpmyadmin`).
  Adminer remains selectable for anyone who prefers a single-file manager.
- **The catalogue carries phpMyAdmin 5.2.3** for all five platforms. The
  digest is deliberately **not** pinned: `lambo db install-ui` therefore fails
  closed, exactly as it must. No checksum was invented to make the gate pass.
- **The manager directory is `<home>/dbui/`**, not `<home>/dbui/adminer/`. The
  path no longer names a manager, because discovery runs without the config in
  hand and must find whichever one is installed.
- **`lambo db open` prefers `http://localhost/phpmyadmin`** when the project is
  running - Apache is already serving the manager through its alias, so there
  is no second server to start and no second port to remember. The standalone
  loopback server remains the fallback when the project is stopped.

### Added

- **Multi-file manager installation.** phpMyAdmin ships as an archive, so
  `dbui` now extracts rather than only copying. The archive format comes from
  the catalogue, never from the downloaded file's name - a cache entry may not
  carry an extension. Extraction happens beside the destination and is renamed
  into place, so a partial unpack never leaves a half-populated manager that
  discovery would accept as installed.
- **`dbui::open_url_at`** - the aliased URL, carrying the same prefilled
  connection details as the standalone form.
- A `phpmyadmin-5.2.3.zip` **test fixture** with an explicit directory
  structure, so the flattening path is genuinely exercised. It is a stand-in,
  not the real phpMyAdmin, and is never installed by the product.

### Fixed

- Staging for an archive install was created **inside** the destination, so
  clearing a previous version deleted the staging tree before the rename that
  needed it. Staging is now a sibling.
- The manager's own name is carried on `dbui::Plan`, so a failure names the
  manager the user configured instead of whichever one the code mentioned.

## [0.9.0] - a native Windows GUI

`lambo-gui` is a Win32 application, not a mock and not a shell over the CLI. It
calls `lambo_core::app` directly, so every button reaches the same engine the CLI
reaches and the two cannot disagree about what a service is doing.

### Added

- **`lambo-gui`** - a native dashboard: project name, overall state, a PHP /
  Apache / database status list, and four actions - Open localhost, Open
  phpMyAdmin, Start, Stop.
- **A two-second refresh.** The engine has no background threads, so the window
  polls it rather than being pushed to. Nothing runs in the background when the
  window is closed, and closing the window does not stop services - a dashboard
  is not a service supervisor.

### Design notes

- **The split is what makes this reviewable.** Almost everything that could be
  wrong about the dashboard is a decision about text, not a Win32 call, so that
  lives in `view.rs` - ordinary Rust, unit-tested on Linux in CI. `win32.rs` only
  creates controls, pumps messages and forwards button presses. The unverifiable
  surface is therefore mechanical rather than where the branching is.
- **States are shown as states.** A runtime that will not run reads
  `8.4.2 - will not run` with the reason attached, not `8.4 ✓`. `Stop` is
  disabled rather than inert when nothing is running. The database manager is a
  button pointing at `/phpmyadmin`, not a row in the service list.
- **`unsafe_code` stays forbidden for the engine and the CLI.** It is relaxed in
  this crate only, because Win32 is FFI, and `view.rs` re-forbids it so the
  tested half cannot drift into FFI.

### Not verified at runtime

There is no Windows desktop in the development sandbox. The crate is
cross-compiled and type-checked for `x86_64-pc-windows-msvc` and is clippy-clean
there, but **no window has ever been created by this code**. Layout, message
handling and control behaviour need physical Windows testing.

## [0.8.0] - an API a GUI can consume

A GUI must not reach the engine by spawning `lambo` and parsing what it prints.
Parsed text is a contract nobody maintains: a reworded message silently breaks a
screen, and a screen that guesses at state is worse than one that admits it does
not know.

### Added

- **`lambo_core::app`** - the application-level API, and the seam a GUI binds
  to. Structured models (`Dashboard`, `ProjectInfo`, `RuntimeInfo`,
  `ServiceInfo`, `Diagnostic`, `OperationOutcome`, `LogView`), all serializable,
  all free of business logic: every method composes `session`, `doctor`, `php`,
  `dbui`, `workspace` and `logs`.
- **`Dashboard`** returns everything the main screen shows in one call, so a GUI
  is not half-updated while it loads.
- **`Error::hint`** - the fixing command as its own accessor, so a GUI can
  render it as an action rather than parsing `next: …` back out of `details()`.

### Design notes

- **States are enumerations, not booleans.** `ServiceState` distinguishes
  `running` / `stopped` / `not_started`, because a service that was never run is
  not a failure and a fresh install has none of them running. `RuntimeState`
  distinguishes `active` / `installed` / `available` / `unavailable` /
  `corrupt`. Collapsing those loses exactly the information the user came for.
- **Severity is exported as words** (`ok`, `warning`, `error`, `unsupported`),
  not the terminal glyphs `Severity::marker` prints. A GUI styles by name and
  should not have to know what character a fixed-width console shows.
- **No networking or RPC.** The API is in-process. A boundary can be added later
  without the models changing, which is why they are serializable now.

### Tests

7 new tests assert the contract a GUI codes against: field presence after
serialization, the state vocabulary, that every non-`ok` diagnostic carries a
runnable fix, and that a runtime which will not run is reported as unhealthy
rather than active.

## [0.7.0] - localhost-first

The product promise is `http://localhost`, not `http://localhost:8080`. A port
that exists only because of an implementation constraint should not appear in
every URL, screenshot and conversation about a project.

### Changed

- **Ports default to 80 / 443** instead of 8080 / 8443. Windows has no
  privileged-port rule, so a standard user binds 80 with no elevation. On Linux
  and macOS, where ports below 1024 need root, Lambo falls back to 8080/8443
  automatically and says so - the first run still needs no administrator rights.
  See [ADR-0007](docs/adr/0007-localhost-first-ports.md), which supersedes
  ADR-0005.
- **The database manager is mounted at `/phpmyadmin`** on the project's own port
  rather than served on a port of its own. It is a Lambo-managed application, so
  Apache aliases it into the site; the user-facing address is
  `http://localhost/phpmyadmin`.
- **`lambo server start` reports the port actually bound.** After a fallback the
  configured and bound ports differ, and printing the configured one would point
  at an address nothing is listening on.

### Added

- **`port::resolve_listen_port`** observes the real `bind` error rather than
  guessing from the platform. "Port 80 must be a permissions problem on Unix"
  would be wrong on Windows, and wrong on Unix too when something is genuinely
  listening there. Refused, occupied and otherwise-unavailable are distinguished
  because they have different remedies.
- **`apache::Plan::aliases`** mounts a directory outside the document root at a
  URL path. `mod_alias` is loaded only when something is actually mounted, and
  each alias gets its own `<Directory>` block - `Require all granted` on the
  document root does not reach a directory outside it, so omitting that makes
  every request to the mounted path a 403.
- **`session::effective_url`** is now the single source of truth for the project
  URL; `lambo open` and `lambo status` both go through it.

### Fixed

- **`lambo doctor` printed prose where a command belongs.** The ports check put
  the conflict explanation in `Check.fix`, which renders as
  `fix: Port 80 is already in use.` - a command that does not exist. Latent
  since the check was written; the new default port is what made it fire. The
  advice now lives in `detail` and `fix` carries something runnable.

### Not in this release

The database manager is still Adminer under the hood, now routed at
`/phpmyadmin`. Replacing it with phpMyAdmin proper is the next step, and needs a
multi-file artifact plus a pinned digest that this environment cannot fetch.

## [0.6.0] - a runtime is not installed until it runs

Every check before this release was about files: the digest matched, the archive
unpacked safely, `bin/php` was on disk. A runtime could pass all of them and
still not execute - a Windows build unpacked on Linux, a binary linked against a
library this machine does not have, an archive that verified perfectly and
contained the wrong thing. `lambo doctor` reported those as healthy, and
`lambo up` failed later with a message that did not point at the real cause.

### Added

- **`RuntimeHealth`.** Starts a runtime and records what it reported: the version
  PHP itself printed, the `php.ini` PHP says it loaded, and its module list. The
  evidence is kept rather than collapsed into a bool, so a failure comes with
  PHP's own message instead of "PHP is broken".
- **Post-install verification.** `lambo php install` starts the binary once
  before reporting success, and removes the runtime if it will not run. The
  error states that the archive verified against its digest, so the user does not
  spend the evening re-downloading a file that was never the problem. Runs only
  for the host platform - a runtime installed for another platform is not
  expected to execute here, and calling that a failed install would be false.
- **`lambo php current` reports evidence**, not just paths: `reported` is the
  version PHP gave when Lambo started it, `configuration` is the file PHP says it
  loaded, `extensions` is how many loaded. A runtime that will not run exits
  non-zero with the reason.
- **`lambo up` starts PHP before it starts the server.** Resolving a runtime is
  not the same as it running: a runtime installed before this release, or one
  whose binary broke afterwards, still resolves - the directory is there, the
  manifest is intact, the executable exists. `lambo up` would previously start a
  web server on it, see the port answer, and report the project as serving while
  PHP could not execute a single request.
- **`lambo doctor` starts PHP.** It previously read the filesystem and the module
  list, so a runtime whose binary would not start reported itself as
  `PHP 8.4.2 (/path)` - a green tick over something that cannot serve a request.
  It now also warns when PHP loaded a configuration file other than the one Lambo
  generated, which is how a broken `PHPRC` becomes visible instead of leaving
  every setting Lambo wrote inert.
- **`docs/runtime.md`** - the install pipeline, per-platform layout, how the
  generated `php.ini` reaches PHP, and what each field of `lambo php current`
  means. Every code block is real captured output.
- **A fixture `php` that executes.** It implements the command surface Lambo
  drives (`-v`, `--ini`, `-m`, `-r`, `-S`, script paths) and refuses everything
  else loudly, so `PHPRC` wiring, health checking, script execution and the
  install gate are exercised end to end. A second fixture archive verifies
  against its digest and then refuses to start, which is precisely the case the
  gate exists for.
- **16 new tests**: 10 that install a runtime which actually runs and assert on
  what it reported, and 6 that drive `lambo php` through the real install
  pipeline.

### Changed

- **`lambo php modules` no longer guesses.** An empty module list used to print
  "PHP reported no extensions - it may be broken" for every failure, because that
  list was all it had. It now says which happened: would not start, started and
  failed, or genuinely has no extensions.
- **One `PHPRC` contract.** `serve_spec` passed the ini *file* while `run` and
  `capture` passed its *directory*. PHP accepts both, so nothing caught the
  disagreement - but two call sites drifting on a contract is how a future change
  breaks one of them. One helper, one form, asserted by a test.

### Fixed

- **`lambo db open` served Adminer without Lambo's generated `php.ini`.** The
  database manager builds its own `php -S` command line rather than reusing
  `php::serve_spec`, and it never set `PHPRC`. Adminer is a PHP application, so
  it was served by a PHP that had never been told where its extensions live - no
  `mysqli`, no `pdo_mysql`, and a login form that could not reach the database it
  exists to manage. The `PHPRC` helper is now shared by both call sites, which is
  the same one-contract fix applied to `serve_spec` above.
- **`lambo php current` printed a directory in a column headed `executable`**
  when no PHP binary could be found. The field is now `Option` and reads
  `not found`.
- **An install error read "exited with exit 127."**

### A note on what is verified

A green fixture run proves Lambo's plumbing. It is **not** verification against a
real PHP build, and nothing in the output claims otherwise. `real_artifacts` now
asserts the things only a genuine interpreter can confirm - that real PHP reports
the declared version, loads Lambo's generated `php.ini`, and lists its
always-compiled-in modules - and otherwise says so and skips.

## [0.5.0] - production runtime distribution readiness

The installer is now ready for real artifacts. The catalogue still ships with no
pinned digests - that work needs network access to the upstream hosts, which
this environment does not have - but the mechanism is complete and tested: a
maintainer can supply a real artifact and a real digest without changing the
installer architecture, `lambo doctor --release` says exactly which entries block
a release, and an offline or mirrored install gets the same verification
guarantee as a downloaded one.

### Added

- **Artifact sources.** `lambo_core::sources` decides where an artifact comes
  from, in a fixed precedence: an explicit catalogue override, a local artifact
  found by its identity-derived name, a configured mirror, then the official
  catalogue URL. The bytes may come from anywhere on that list; the *expected
  digest* may only come from trusted metadata - the digest pinned in the
  catalogue, or the `.sha256` sidecar published beside the **official** URL. A
  mirror never supplies the digest for the bytes it serves, and neither does a
  directory of local files.
- **`sources.mirror` and `sources.artifacts`** configuration keys, both empty by
  default. A mirror is addressed by artifact name, so it is a flat directory of
  files - what an internal artifact store or a share actually is.
- **Provenance in the install manifest**: `source_kind` records `catalogue`,
  `override`, `mirror`, `local`, or `unknown`. Read back with a serde default,
  so a manifest written by an earlier Lambo still parses and reports `unknown`
  rather than claiming the official catalogue.
- **`lambo doctor --release`**, the catalogue release gate. `lambo doctor` plus
  the rule that an entry with no pinned digest is an error rather than a fact to
  work around. Validates metadata only; downloads nothing.
- **`lambo doctor` runtime readiness**, which separates what the catalogue
  *lists* from what can actually be installed. "2 PHP releases available" above
  a command that then fails closed is misleading.
- **`lambo config hash <file>`** computes a SHA-256 for pinning. It prints and
  stops: recording the digest of whatever it was handed would turn every install
  into trust-on-first-use.
- **`lambo php list` SOURCE column**: official, mirror, override or local, plus
  whether the artifact can be verified at all.
- **Real-artifact testing** in `tests/fixtures/real-artifacts/`. Supply a
  manifest and an archive and the full install pipeline runs against them; with
  none present the test passes immediately, so CI never depends on the
  directory. Everything but the README and example is git-ignored.

### Changed

- **`file://` now works in production.** The production downloader hard-coded
  `--proto =https`, so it could not fetch a local artifact at all and only the
  test-only downloader could. Offline installation was therefore impossible
  however the catalogue was configured. `SystemDownloader` routes `https://` to
  `curl` and `file://` to a direct copy, both into the same cache verified by the
  same code.
- **`write_manifest` takes the resolved source** rather than eight positional
  parameters, four of them `&str`. Swapping the URL and the digest at a call
  site used to compile cleanly and record a digest as a URL.

### Fixed

- **Three hints named configuration keys that do not exist.** `lambo config set
  php.<version>.sha256`, `lambo config set php.path` and `lambo config set
  dbui.sha256` have never been in `Config::KEYS`; the command answers "unknown
  configuration key". The advice described a fix that could not be applied,
  which is worse than no advice because it sends somebody to a dead end with
  confidence. All three now name the mechanism that exists - an entry in
  `config/catalogs/` with a pinned `sha256` - through one shared constant.

### Security

- There is no `--skip-checksum`, `--insecure`, `--no-verify` or equivalent, and
  no environment variable that disables verification. The design principle is
  unchanged: no digest, no installation.
- A local artifact is verified exactly like a downloaded one. A name Lambo
  cannot prove is a single safe path component is not used at all, so a version
  or platform containing `..` cannot reach outside the artifacts directory.

## [0.4.0] - verified localhost lifecycle

Lambo's service lifecycle is now verified end to end rather than asserted from
its parts: `lambo up` binds a port, a real HTTP request against it returns 200,
`lambo status` reports the service as running only while it actually answers,
and `lambo down` stops the process, releases the port, and leaves no orphan.
Writing those tests found four real bugs, each of which is fixed here.

Because real PHP, Apache and MariaDB artifacts cannot be downloaded in CI, the
server in the lifecycle tests is a small fixture that accepts the same arguments
as `php -S`. Everything around it is the production code path. What that leaves
unverified - real `mod_php`, real MariaDB initialization, a real desktop browser
- is documented in [roadmap.md](docs/roadmap.md#what-verified-means).

### Added

- **The localhost lifecycle is verified end to end.** `tests/lifecycle.rs`
  drives the production orchestration against a real child process and asserts
  what a user would observe: `up` binds a port, a real HTTP request returns 200,
  `status` reports the service running only while it answers, `down` stops it,
  the port stops accepting and is released, and no orphan survives. Repeated
  `up`/`down` cycles, `restart`, port reuse, a server that exits immediately, a
  server that binds but never answers, an occupied port, and reconciliation of a
  stale record are all covered.
- **The CLI is driven as a user drives it.** `crates/lambo-cli/tests/cli_lifecycle.rs`
  runs the real `lambo` binary through `init → status → up → status → down →
  status`, asserting on exit codes and output with a real HTTP 200 in the
  middle.
- **Windows path coverage.** `tests/windows_paths.rs` asserts a Lambo home under
  `C:\Program Files\Lambo`, a project at `C:\Projects\my project`, and
  generated `httpd.conf` keeping spaces intact and quoted - all while running on
  Linux, because `Os` is an argument rather than a compile-time switch.
- **A deterministic HTTP fixture** (`lambo-fixture-server`) that accepts the same
  arguments as `php -S`. It is test-only: never in the catalogue, never in a
  release archive.

### Fixed

- **`lambo down` could leave its own service running.** Every start path read the
  child's identity immediately after spawn, which on Unix can land before `exec`
  replaces the forked copy of Lambo - recording *Lambo's* image name. The start
  time was already final, so nothing else looked wrong, but the name never
  matched a later probe, `lambo down` concluded the PID belonged to somebody
  else, signalled nothing, and removed the record. The service kept running with
  no way to stop it. Identity is now read after `exec` settles, and if it still
  has not, no name is recorded at all: a missing name only weakens the
  comparison, whereas a wrong one proves a falsehood.
- **Health checks refused healthy servers.** The HTTP client used only the first
  address a hostname resolved to. `localhost` commonly resolves to `::1` before
  `127.0.0.1` while Lambo's servers bind IPv4 explicitly, so `lambo up` reported
  "the server started but never answered an HTTP request" about a server that
  was serving fine. Every resolved address is now tried.
- **Advice that could not work.** `lambo init` writes `server.kind: apache` into
  `lambo.yml`, and a project file overrides global configuration - so when no
  Apache build exists for the platform, the error told the user to run
  `lambo config set server.kind php`, which writes globally and is then ignored
  by the project file that caused the failure. All four sites now name
  `lambo.yml` first and explain the precedence.
- **`ProcessSpec::render` produced a command line that could not be re-run.** It
  quoted arguments containing spaces but not the program path, so a Windows
  install under `C:\Program Files\...` broke at the first space - despite the
  method's own doc comment promising the output was re-runnable.


- **A resolved runtime view.** `php::Runtime` resolves an installed PHP into
  the concrete paths a service needs - executable, generated `php.ini`,
  extensions directory - for one platform, and refuses to construct itself when
  the executable is missing. An interrupted install can no longer be presented
  as usable.
- **`php::version_table`** merges the local inventory with the catalogue.
  `lambo php list` now prints `VERSION / STATUS / PATH` with each version
  marked `active`, `installed`, `available` or `unavailable`. A release
  published only for another platform is shown with the platforms it does exist
  for, rather than being dropped.
- **`doctor` reports `UNSUPPORTED`**, ordered below a warning. Where the
  catalogue publishes nothing for the running platform - Apache is Windows-only
  and MariaDB/MySQL have no macOS entry - the check says so and names the
  workaround instead of reporting a failure the user cannot fix. A platform
  limitation does not turn `lambo doctor` red.
- **Process identity.** `ProcessIdentity` captures a process's boot-relative
  start time and image name; every start path records it, and `stop_verified`
  re-reads it before signalling. A PID that has been reused by an unrelated
  program returns `NotOurs` and nothing is signalled.
- **`ui.table`**, an aligned column renderer used by `lambo php list`.
- **Install manifests.** Every runtime install writes `.lambo-install.json`
  into the runtime directory recording the family, version, platform, source
  URL, the digest it was verified against, and the file list at completion. A
  directory with no manifest was never finished.
- **`lambo php list` reports `corrupt`.** A runtime whose files no longer match
  its manifest - an emptied `ext/`, a renamed directory, the wrong family - is
  reported as `corrupt` instead of `installed`. Integrity outranks selection,
  so an active-but-broken runtime never reads as `active`.
- **`doctor` validates the catalogue.** Blocking problems - duplicate entries,
  unparseable versions, unknown platform keys, non-HTTPS URLs, formats Lambo
  cannot unpack, malformed digests - fail the check before it counts anything,
  and the message says whether the entry came from your override file or from
  the catalogue Lambo ships, because those need opposite fixes.
- **Catalogue entries can describe their artifact.** Optional `archive_format`,
  `filename`, `size`, `channel` and `executable` fields, all defaulted so every
  existing catalogue keeps loading.
- **Committed test artifacts with real digests.**
  `crates/lambo-core/tests/fixtures/runtime/` holds a linux `.tar.gz` and a
  Windows `.zip` whose SHA-256 values are committed beside them and asserted to
  match before any test uses them. A digest that has drifted from its archive
  now fails the build instead of reaching a user.
- **`runtime::verify` and `Integrity`**, the reusable check behind `corrupt`,
  and **`runtime::promote`**, the shared staging-to-final move.
- **A rejected download no longer leaves a `.part` file in the cache.**
  Verification failed with an early return, so the partial artifact survived on
  disk.
- Checksum messages now read `checksum verification failed` for a digest that
  does not match, and `cannot verify` for an artifact with no digest available
  anywhere. Both say plainly that nothing was fetched or executed.

### Changed

- **`download_verified` returns the digest it actually used**, not just a path.
  An unpinned release verifies against the `.sha256` published beside the
  artifact; the install manifest previously recorded the catalogue field, which
  is empty in that case, and so would have claimed nothing was verified for an
  install that was.
- **An unverifiable artifact has its own error.** `Error::VerificationUnavailable`
  carries the URL, family, version, platform and remedy as fields. It replaces a
  `Download` failure whose prose the database-manager install path
  pattern-matched on the substring `checksum is unavailable` to recover its own
  hint.
- **An upgrade that fails leaves the previous runtime in place.** The install
  paths deleted the installed directory before renaming staging over it, so a
  rename failing afterwards - a locked file on Windows, a full disk - left no
  runtime at all. The previous directory is now moved aside and restored if the
  rename fails.
- **`lambo php remove` refuses a runtime a live Lambo service is using** and
  names `lambo down` as the fix. Deleting files under a running `httpd` does not
  stop it; it keeps serving from unlinked files and then cannot be restarted.
  A stale record whose process has exited does not block the removal.
- **A runtime's version is no longer taken from its directory name.** Discovery
  still lists a hand-placed directory, but a manifest that disagrees with the
  name is reported rather than believed.
- **`php::install_release` rejects an unknown platform key** instead of
  silently splitting the string and defaulting to Linux, which gave a Windows
  install a Unix layout that then reported itself broken.
- **`lambo status` resolves the project's PHP requirement** and reports the
  version that will actually serve it (`stable → 8.4.2 installed`), rather than
  only echoing the requirement.
- **Fix hints audited against the real command tree.** Four suggested commands
  did not exist and no longer appear: `lambo setup`, `lambo server install`
  (two places), `lambo up --open`, and `lambo up --workspace <name>`.

### Removed

- **The MySQL 8.0.40 `linux-x64` catalogue entry.** It pointed at a `.tar.xz`,
  which Lambo has no decompressor for, so it downloaded and verified correctly
  and then could not be unpacked. It has never been installable. MariaDB covers
  `linux-x64` and `linux-arm64`.

## [0.2.0] - Lambo PHP

The project was renamed from king-PHP to **Lambo PHP**, and the scope moved
from "workspace manager" to a working local development environment: PHP,
Apache and MariaDB installed and supervised by Lambo itself, with Windows as
the primary platform.

### Changed

- **Rename.** Product `Lambo PHP`, binary `lambo`, crates `lambo-core` /
  `lambo-cli`, project file `lambo.yml`, environment variable `LAMBO_HOME`,
  default home `%USERPROFILE%\Lambo` (Windows) and `~/.lambo` (elsewhere).
- **`KING_HOME` is no longer honoured** for locating the home directory. A new
  install must never inherit a legacy location by accident; the legacy home is
  read only by `lambo migrate`.
- `server.kind` and `database.kind` in `lambo.yml` are now optional, so an
  omitted value inherits the global configuration instead of pinning a value
  the project did not ask for.
- The download verifier resolves the expected checksum **before** downloading,
  so an unverifiable release is refused without fetching it.

### Added

- **Service managers** - `apache` (generated `httpd.conf`, validation,
  graceful shutdown), `database` (MariaDB/MySQL: generated `my.cnf` bound to
  `127.0.0.1`, initialization, generated root password passed via `MYSQL_PWD`
  rather than argv, create/drop/list/shell), `dbui` (Adminer on loopback),
  `browser` (no shell on any platform), `envfile` (non-destructive `.env`
  editing), `logs`.
- **Orchestration** - `session::up/down/status` with a fixed, ordered
  pipeline: validate → credentials → port preflight → PHP → database → `.env`
  → server → HTTP health → browser. State is saved after every start, so a
  failure never leaves an unrecorded process.
- **CLI surface** - `lambo init|up|down|restart|status|open|doctor|logs|
  migrate`, `lambo php list|list-versions|install|use|current|remove|ini|
  modules|run|-v`, `lambo server start|stop|restart|status`, `lambo db
  install|install-ui|start|stop|restart|status|create|drop|list|shell|open|
  credentials`, `lambo config show|get|set|list|path|edit`, `lambo workspace
  list|add|remove`.
- **`lambo migrate`** - one-way upgrade of configuration, workspaces and every
  project file from a king-PHP installation. Deletes nothing, copies no
  runtimes, idempotent.
- **Packaging** - per-user Windows installer (`LamboPHP-Setup.exe`, NSIS),
  portable archives per platform, `SHA256SUMS` verified by the release
  workflow.
- **CI** - test / clippy / fmt on Windows, Linux and macOS; cross-compile
  checks for `x86_64-pc-windows-msvc` and `aarch64-unknown-linux-gnu`; a
  dedicated Windows behaviour job.
- **Docs** - installation, Windows guide with a manual smoke-test checklist,
  troubleshooting, development, release, rewritten command and configuration
  references, ADR-0006.

### Fixed

- **`terminate_tree` no longer signals a process group derived from a recorded
  pid.** A recycled group id could name processes Lambo has never seen - on one
  host it killed the shell that started it. Unix now signals the bare pid;
  Windows keeps `taskkill /T`, which the OS scopes to the tree.
- `lambo php -v` was rejected by the parser; hyphenated arguments now reach
  PHP, while `--help` and `--version` stay with Lambo.
- `lambo doctor` suggested `lambo server install` and
  `lambo composer install`, neither of which exists.
- `runtime::is_executable_file` used `std::os::unix` unconditionally, breaking
  the Windows build.
- Archive extraction pre-sizes its buffer from the declared entry size instead
  of collecting bytes one at a time.

### Removed

- `lambo repair` - it depended on a self-healing check API that the reworked
  diagnostics do not have. `lambo doctor` names the command that fixes each
  finding instead.
- The "planned command" stubs. A command either exists or it does not appear in
  the CLI; nothing exits successfully while doing nothing.

### Known gaps

- The catalogue ships with `sha256: null` for every entry, so downloads fail
  closed until a checksum is pinned. See
  [docs/installation.md](docs/installation.md#pinning-checksums).
- No Apache or MariaDB catalogue entries for macOS; no Adminer checksum.
- No TLS yet: `server.https_port` is reserved but unused.
- `clap` is built without colour support in this tree because the `anstream`
  dependency chain could not be vendored offline; `lambo` still honours
  `NO_COLOR` and detects a TTY through its own UI layer.

## [0.1.0] - milestone M1 (Foundation)

### Added

- **Core engine (`king-core`)** - all business logic, UI-free:
  - `paths` - single-home filesystem layout under `KING_HOME`
    (default `~/.king`; portable mode is just an env var)
  - `config` - global `king.yml`: `php.default`, `server.*`, `database.*`,
    `ssl.auto`; validated `get`/`set` for every interface
  - `version` - PHP version specs: `stable`, `latest`, `nightly`, `8.4`
    (release line), `8.4.2` (exact), full semver expressions
  - `kingfile` - per-project `kingphp.yml` with forward-compatible schema,
    upward project-root discovery
  - `detect` - evidence-based framework detection (Laravel, Symfony,
    CodeIgniter, CakePHP, Slim, Yii, Drupal, Magento, WordPress, generic
    Composer, plain PHP) with suggested kingfile generation
  - `runtime` - installed-runtime registry: semver inventory, best-match
    selection, activation via `.active` marker files
  - `doctor` - diagnostic checks (layout, writability, port availability,
    runtime inventory, stale activation markers) with automated repair
  - `workspace` - named project groups with a versioned on-disk registry
- **CLI (`king`)** - thin shell over the core engine:
  - `king` / `king status`, `king init`, `king doctor`, `king repair`,
    `king config show|get|set|path|edit`, `king php list|use`,
    `king workspace add|list|remove`
  - `king up|down|restart` and `king php install` recognized as roadmap
    commands that fail loudly with a milestone pointer
  - `NO_COLOR`-compliant, TTY-aware output
- **Project infrastructure**
  - CI: rustfmt, clippy (`-D warnings`), tests on Linux/Windows/macOS, MSRV job
  - Release workflow: tag-triggered cross-platform binaries with SHA256 sums
  - `scripts/install.sh` curl installer
  - Docs: architecture, ADRs 0001–0005, roadmap, command/config/kingfile
    references, FAQ
  - Issue/feature/discussion templates, contributing guide, CoC, security policy

[Unreleased]: https://github.com/flessan/lambophp/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/flessan/lambophp/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/flessan/lambophp/compare/v0.4.0...v0.5.0
[0.1.0]: https://github.com/flessan/lambophp/releases/tag/v0.1.0
