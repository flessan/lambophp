# ADR-0006: Windows as a modelled platform, and never signalling a foreign process group

- **Status:** Accepted
- **Date:** 2026-09-11

## Context

Lambo's headline platform is Windows 10/11, and its job is to start and stop
long-lived processes it does not own the lifetime of: `httpd`, `mysqld`,
`php -S`, Adminer. Two decisions follow from that, and both were forced by
observed behaviour rather than by taste.

**1. Platform differences must be data, not compilation.**

The first instinct - `#[cfg(windows)]` at each point of difference - makes
Windows code untestable on the machines where the test suite runs. A bug in a
`cfg(windows)` branch is discovered by a user, not by CI.

**2. Stopping a service must not be able to hit a stranger's process.**

The original implementation recorded a pid, and on shutdown signalled the
*process group* derived from it (`kill -TERM -<pid>`). A pid read back from a
state file may be stale, and a recycled pid can name a group Lambo has never
seen. On at least one real host this killed the shell that started it.

## Decision

**Platform model.** Every platform decision goes through the `Os` value
(`platform::Os`), which is an argument to the functions that need it:

```rust
pub fn is_executable_file(path: &Path, os: Os) -> bool
pub fn command_spec(plan: &Plan, config_file: &Path, log: &Path) -> ProcessSpec
pub fn initialize_spec(database: &Database, plan: &Plan, os: Os) -> Option<ProcessSpec>
pub fn opener(os: Os, url: &str) -> (ProcessSpec, Option<ProcessSpec>)
```

Tests therefore assert the exact Windows command line, `.exe` suffix, and
forward-slashed configuration path while running on Linux. `#[cfg]` appears
only where the operating system genuinely has no portable equivalent:
`CREATE_NEW_PROCESS_GROUP` when spawning, `taskkill /T` when stopping, and the
executable-bit check.

**Shutdown.** `process::terminate_tree` signals the recorded pid only. On
Windows it uses `taskkill /T`, where the OS itself scopes the tree. Group
signalling was removed outright, with a test
(`stopping_one_service_leaves_every_other_process_alone`) asserting that an
unrelated process survives a stop.

## Rationale

- Windows behaviour is covered by the ordinary test suite on every platform,
  so a Windows-only regression fails CI on Linux.
- The blast radius of a stop is bounded by what Lambo recorded, never by what
  the kernel happens to group together.
- `taskkill /T` gives correct tree semantics on the platform where tree
  semantics matter most (Windows services spawn children freely).

## Consequences

- **Accepted:** a Unix child that double-forks out of the recorded tree can
  survive `lambo down`. The alternative - group signalling - can kill
  processes Lambo has never seen, which is a strictly worse failure.
- **Accepted:** functions take an `Os` argument even when the caller always
  passes `Os::host()`. That parameter is the test seam; removing it would
  remove the ability to test Windows anywhere but on Windows.
- Every new platform-dependent behaviour needs a rule in `Os` and a test that
  asserts the non-host platform's answer.
- `lambo doctor` and the Windows smoke-test checklist
  ([docs/windows.md](../windows.md)) exist because process supervision is the
  one area a unit test cannot fully prove.
