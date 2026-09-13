# ADR-0002: Monorepo Cargo workspace with layered crates

- **Status:** Accepted
- **Date:** 2026-07-30

## Context

The architecture mandates strict layering (GUI/TUI/CLI are thin shells over
a core engine). Options: one crate with modules, one repo per layer, or a
Cargo workspace in one repo.

## Decision

**A Cargo workspace in a single repository.** Crates under `crates/`:
`lambo-core`, `lambo-cli`, later `lambo-tui` and the GUI. Internal crates are
`publish = false`; versioning is unified at the workspace level for now.

## Rationale

- **Enforced boundaries.** Crate edges are compile-time walls: the CLI
  *cannot* reach into core internals, unlike modules in one crate where
  `pub(crate)` discipline erodes.
- **One lockfile, one CI.** Cross-cutting changes (config schema ↔ CLI ↔
  docs) ship atomically in one PR.
- **Independent compilation.** Interface crates build in parallel after
  the core; contributors touch one crate at a time.

## Consequences

- Splitting into separate repos later is always possible (crates are
  self-contained); merging later would be much harder. This direction is
  the reversible one.
- Releases tag the whole workspace (`v0.2.0` means "lambo CLI + engine"),
  keeping compatibility stories simple for users.
