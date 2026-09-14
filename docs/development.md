# Development

## Prerequisites

- Rust 1.85 or newer (the workspace is edition 2024). `rust-toolchain.toml`
  pins the version CI uses.
- No C toolchain, no system libraries, no network access once crates are
  vendored.

```bash
rustup show          # confirm the pinned toolchain
cargo --version
```

## Getting started

```bash
git clone https://github.com/flessan/lambophp.git
cd lambophp

cargo test --workspace --all-targets     # the whole suite
cargo run -p lambo-cli -- --help         # the CLI
cargo run -p lambo-cli -- doctor
```

Work against a throwaway home so your real machine is untouched:

```bash
export LAMBO_HOME="$(pwd)/.lambo"     # git-ignored for exactly this purpose
cargo run -p lambo-cli -- status
```

On Windows:

```powershell
$env:LAMBO_HOME = "$PWD\.lambo"
cargo run -p lambo-cli -- status
```

## The checks that must pass

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc
```

The last one matters as much as the others: it is what catches a Unix-only
import or a Windows-only API before a Windows user does. CI runs it in both
directions (Windows target on Linux, Linux target on Windows is unnecessary
because the Linux target builds natively there).

`unsafe_code` is `forbid`den workspace-wide. `std::env::set_var` is `unsafe` in
edition 2024, so PATH-dependent code takes the search path as an *argument*
rather than mutating the environment - which is also what makes it testable.

## Adding a command

The checklist is short because the architecture does the work.

1. **Logic first, in `lambo-core`.** A new rule, schema or condition belongs in
   a module there, with unit tests. If you cannot name the core function your
   command calls, the logic has leaked into the CLI.
2. **Declare it** in `crates/lambo-cli/src/cli.rs`.
3. **Implement** `crates/lambo-cli/src/commands/<name>.rs`: parse nothing,
   decide nothing, render through `crate::ui::Ui`.
4. **Wire the match arm** in `cli::run`.
5. **Test** the core behaviour in the module; add a CLI test when the argument
   handling itself has a rule (see `cli::tests` for the `lambo php -v`
   rewrite).
6. **Document** it in `docs/commands.md`, and in the README table if it is
   user-visible enough to belong there.

## Adding a diagnostic

1. Write `doctor::check_<thing>` in `lambo-core` - pure observation, returning
   a `Check`.
2. Every `warn` and `fail` **must** carry the command that fixes it. There is a
   test that fails the build if one does not.
3. Add it to `doctor::run`.
4. If the fix is a new command, add the command first.

## Conventions

- **No shell, ever.** A process is a program plus an argument vector
  (`process::ProcessSpec`). Never interpolate user input into a command line.
- **Platform differences go through `Os`.** Add a rule to `platform::Os` and a
  test asserting the non-host platform's answer. See
  [ADR-0006](adr/0006-windows-as-a-modelled-platform.md).
- **Never claim success you did not observe.** A service is up when its port
  answers, not when a process started.
- **Fail closed.** No checksum, no execution. No record in the state file, no
  process gets signalled.
- **Errors carry causes.** Use `Error::ServiceFailed` with its `causes` and
  `hint` rather than a bare message; the CLI renders them.
- **Atomic writes.** Configuration and state go through `fsx::write_atomic`.

## Testing notes

Some things this codebase learned the hard way, encoded as tests:

- **Test servers must not accept more connections than the client makes.** A
  server looping on `accept()` forever hangs the entire suite.
- **No test may signal a process group.** On some hosts that kills the test
  runner itself; `terminate_tree` does not do it in production either.
- **Never write a test vector from memory.** Digests are generated with
  `sha256sum`/`python3 -c`, not recalled.
- **`Path::starts_with` and `PathBuf::join` are component-based**, not
  string-based: on Linux, joining `C:\Users\dev` with `Lambo` yields
  `C:\Users\dev/Lambo`. Tests that assert Windows path shapes account for this.
- **`testutil::home()` creates the layout.** A test that needs a *missing* home
  builds one with `Paths::from_root(...)`.
- `LocalDownloader` serves `file://` fixtures through the same code path as
  `curl`, so checksum verification and archive extraction are genuinely
  exercised offline.
- **A synthetic archive that differs from the real one tests the wrong path.**
  The committed fixtures carry explicit directory entries because real PHP
  archives do, and `single_top_level_dir()` only collapses a wrapper directory
  when the archive has them. The first version of the tar fixture omitted them
  and the wrapper survived, installing as `php/8.4.2/php-8.4.2/bin/php`.

### The committed fixtures

`crates/lambo-core/tests/fixtures/runtime/` holds two real archives - a linux
`.tar.gz` and a Windows `.zip` - with their real SHA-256 values committed in
`catalogue.json` and in `sha256sum`-format sidecars beside them.
`tests/runtime_distribution.rs` asserts all three agree *before* it uses any of
them, so editing an archive without updating its digest fails the build rather
than silently weakening every pinned-download test.

They exist because in-memory fixtures hash the archive on the spot: that proves
the code works but says nothing about whether a digest in a catalogue file
matches the bytes it claims to describe, which is the failure that reaches
users.

Both are byte-reproducible - fixed member timestamps, and stored rather than
deflated zip entries so the digest cannot drift with the zlib version. If you
regenerate one, update the digest in `catalogue.json`, the `.sha256` sidecar,
and the constants in `the_fixtures_are_reproducible`.

They describe a PHP that does not exist. They are **not** a real PHP build and
must never be referenced from `catalogs/default.json`.

### The lifecycle fixture server

`lambo-fixture-server` is a small deterministic HTTP server used by
`tests/lifecycle.rs` and `crates/lambo-cli/tests/cli_lifecycle.rs`. It accepts
the same arguments as PHP's built-in server:

```text
lambo-fixture-server -S 127.0.0.1:8080 -t /path/to/docroot
```

That argument shape is the whole point. A test installs it as the `php`
executable of a runtime, and then `session::up` drives it through
`php::serve_spec`, `process::spawn`, `http::wait_until_up`, `State` and
`session::down` - all production code, unchanged. Nothing in the tests
reimplements the lifecycle, which is what keeps the suite honest about the thing
it claims to verify.

It also answers the three probes Lambo makes of a PHP binary before it will run
one - `-v`, `--ini` and `-m` - in PHP's own output shapes, honouring `PHPRC` the
way PHP does. This was not optional: Lambo now starts a runtime to confirm it
works, and correctly refused a stand-in that modelled only `-S`. Being installed
as `php` means answering what is asked of `php`. The parsers those probes feed
are the same ones the real-artifact test checks against a genuine PHP build, so
a drift between the fixture's output and PHP's shows up there.

A control file beside the binary - `.fixture-version` - overrides the version it
reports, so a test can make one specific installed runtime disagree with its
catalogue entry without touching the process environment.

Two failure modes are selectable with a marker file in the document root,
because the command line is fixed by `serve_spec`:

- `fixture-exit` - print a diagnostic to stderr and exit non-zero without
  binding, which is what a server with a broken configuration does.
- `fixture-silent` - bind the port and accept connections but never send a
  response. This is the case a listening-socket check reports as healthy and a
  real HTTP request exists to catch.

It is a `[[bin]]` of `lambo-core` so Cargo exposes it to tests through
`CARGO_BIN_EXE_lambo-fixture-server`. `cargo build --release` builds it, but
`scripts/package.sh` copies only the `lambo` binary, so it cannot reach a
release archive - verified by inspecting the archive contents.

## Repository layout

```
crates/lambo-core/src/     the engine (see docs/architecture.md for the map)
crates/lambo-core/catalogs/default.json   the embedded download catalogue
crates/lambo-core/tests/   integration tests
crates/lambo-core/tests/fixtures/runtime/   committed artifacts + real digests
crates/lambo-cli/src/      the CLI: cli.rs, commands/, ui.rs, error.rs
docs/                      architecture, ADRs, references, guides
packaging/lambo.nsi        the Windows installer definition
scripts/                   install.sh, package.sh, installer.ps1
web/                       the landing page
```

## Release process

[release.md](release.md).
