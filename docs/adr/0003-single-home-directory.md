# ADR-0003: Single home directory (`$LAMBO_HOME`), marker files over symlinks

- **Status:** Accepted
- **Date:** 2026-07-30

## Context

Lambo owns runtimes, configuration, caches, logs, and plugins. Options:
spread across XDG directories (`~/.config`, `~/.local/share`, `~/.cache`),
or one root directory like rustup (`~/.rustup`) and Bun (`~/.bun`).

A secondary decision: how to record the *active* version of a runtime -
symlinks (`current -> 8.4.2`) or a plain marker file.

## Decision

1. **One home directory:** `$LAMBO_HOME`, default `~/.lambo`, with the
   environment variable as the only override.
2. **Marker files** (`runtimes/<kind>/.active` containing a version string)
   instead of symlinks.

## Rationale

- **Portable mode falls out for free.** `LAMBO_HOME=/usb/lambo` gives a fully
  portable installation - a headline feature - with zero extra code. XDG
  scattering would need four overrides.
- **Uninstall stays honest.** `rm -rf $LAMBO_HOME` plus removing one PATH
  entry. No forensic hunt through three dot-directories.
- **No sudo, ever, for state.** Everything is inside `$HOME`.
- **Symlinks are not portable.** Windows symlink creation historically
  requires Developer Mode or admin; a text file behaves identically
  everywhere, survives `rsync`/`tar` copies, and remains human-inspectable.
  The marker is validated on read (stale markers are doctor warnings, not
  crashes), which is strictly more robust than a dangling symlink.

## Consequences

- We consciously depart from FHS conventions; documented in
  [configuration.md](../configuration.md). XDG purists can point
  `LAMBO_HOME` at `~/.local/share/lambo`.
- Consumers must read `.active` via `runtime::active*` and never interpret
  the registry by hand; the module is the single source of truth.

## Amendment (Lambo PHP)

This ADR was written for king-PHP, where `KING_HOME` selected the home. Lambo
keeps the single-home design but **does not read `KING_HOME`**: a new install
must never inherit a legacy location by accident, and two environment
variables resolving to two homes is a bug users cannot diagnose. The legacy
home is read only by `lambo migrate`, which reports what it found and copies
configuration - never runtimes - into the Lambo home.
