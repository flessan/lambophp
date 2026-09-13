# Contributing to Lambo PHP

Welcome - and thank you. Lambo PHP is built to be maintained for a decade by
thousands of hands, so we optimize for *readers* of code, not writers. This
guide explains the rules that keep that true.

## The five rules

1. **UIs contain zero business logic.** CLI, TUI, and GUI parse, render,
   and delegate. Every rule, schema, and condition lives in `lambo-core`.
2. **No untested logic.** Every module ships with unit tests; every
   cross-module flow has an integration test in `crates/lambo-core/tests/`.
3. **Never silently wrong.** Validate at the boundary, fail with context
   (paths, keys, input), and always offer a next action.
4. **Docs are part of the feature.** Behavior changes update the relevant
   file under `docs/` in the same PR. Architectural decisions get an ADR.
5. **Small, focused PRs.** Every PR improves the project; no drive-by
   changes, no unrelated reformatting.

## Setup

```bash
git clone https://github.com/flessan/kink-php-dev.git
cd kink-php-dev
rustup show           # installs the toolchain pinned in rust-toolchain.toml
cargo test --workspace
```

The first `cargo build` produces `Cargo.lock` - **commit it**. This
workspace ships a binary, so the lockfile is tracked on purpose.

## Everyday commands

```bash
cargo fmt --all              # format (CI enforces --check)
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo test --workspace       # unit + integration tests
cargo run -p lambo-cli -- status        # run the CLI
cargo run -p lambo-cli -- --help
```

Point Lambo at a throwaway home while developing so your real machine is
untouched:

```bash
export LAMBO_HOME="$(pwd)/.lambo"   # git-ignored for exactly this purpose
```

## Adding a CLI command - the checklist

1. **Logic first, in `lambo-core`.** New rule? New module or function in the
   core, with unit tests. If nothing new is needed, reuse what's there.
2. **Handler in `crates/lambo-cli/src/commands/<name>.rs`.** Render with
   `Ui`; delegate to the core. Handle `ExitCode` deliberately.
3. **Wire it in `cli::Command`** - one enum variant, one match arm.
4. **Document it** in `docs/commands.md` and the README command table.
5. **Test the flow** if it spans modules: add a case to
   `crates/lambo-core/tests/`.

## Adding a doctor check

1. Implement `doctor::Check` in `lambo-core` (pure observation in `run`).
2. Only override `repair` when the fix is safe, automatic, and idempotent.
3. Register it in `doctor::standard_checks` (order = display order).
4. Cover both states (healthy/broken) with unit tests.

## Commits and PRs

- **Conventional Commits**: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  `ci:`, `chore:` - optionally scoped: `feat(detect): recognize Statamic`.
- Every PR must be green on CI: fmt, clippy, tests on all three OSes, MSRV.
- Fill out the PR template; link the issue it closes.
- The maintainers review for architecture first, code second.

## Architectural decisions

Changing layering, file formats, defaults, or public core APIs? Write an
ADR first (`docs/adr/NNNN-title.md`, copy `0001` as a template) and open a
Discussion. Backward compatibility of `lambo.yml` (and the legacy `lambo.yml`), `lambo.yml`, and
`workspaces.yml` is treated as sacrosanct - evolve them additively, schema
version fields exist for a reason.

## Reporting bugs and vulnerabilities

- Bugs: use the *Bug report* issue template. Include `lambo --version`, OS,
  and `lambo doctor` output.
- Security: **never** a public issue - see [SECURITY.md](SECURITY.md).

## Community

Be kind. The [Code of Conduct](CODE_OF_CONDUCT.md) applies everywhere the
project happens.
