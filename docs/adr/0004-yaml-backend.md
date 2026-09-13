# ADR-0004: YAML for humans, backend hidden behind one module

- **Status:** Accepted
- **Date:** 2026-07-30

## Context

Config files must be human-first (YAML was chosen in the product brief).
The Rust YAML landscape has a wrinkle: `serde_yaml` is archived (though
stable and ubiquitous), and maintained forks (`serde_yml`, `libyml`) are
young. Betting the codebase on any crate's *name* is a liability.

## Decision

1. **YAML everywhere for user-facing files** (`lambo.yml`, `lambo.yml` (and the legacy `lambo.yml`),
   `workspaces.yml`).
2. **The backend crate is named in exactly one module** -
   `lambo_core::yaml` (plus one placeholder in `error.rs`). Every other
   module calls `yaml::to_string` / `yaml::from_str`.
3. Ship with `serde_yaml` 0.9 today: stable, audited by usage volume, and
   interchangeable behind our seam.

## Rationale

- **Swap cost = one file.** If the ecosystem converges on a fork, one PR
  changes the backend; call sites never learn about it.
- **Header comments** are prepended manually at write time (serde YAML
  emits no comments), keeping files self-documenting without coupling the
  schema to the emitter.

## Consequences

- Custom YAML features (anchors, tags) are out of scope deliberately; our
  schema is plain maps/lists/scalars, so any backend can serve it.
- Deprecation warnings about `serde_yaml` in the ecosystem are a docs
  topic, not a fire: the seam is the mitigation.
