<!--
Thanks for improving Lambo PHP. A PR is reviewed architecture-first,
code-second: a small, correct, well-placed change beats a big clever one.
-->

## What & why

<!-- What does this change, and what problem does it solve? Link the issue:
Closes #___ -->

## Where the logic lives

<!-- The five rules (CONTRIBUTING.md): UIs contain zero business logic.
Confirm where the behavior changed and why that is the right place. -->

- [ ] Business logic (if any) is in `lambo-core`; interfaces only parse/render/delegate
- [ ] No existing public behavior changed silently (or it is justified below)

## Verification

- [ ] `cargo fmt --all` applied
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean
- [ ] `cargo test --workspace` passes (new logic has new tests)
- [ ] Docs updated (`docs/`, README table, CHANGELOG.md) where behavior changed

## Risk & rollout

<!-- What could break? How does a user recover? Backward compatibility of
lambo.yml / workspaces.yml (and the legacy kingphp.yml) is sacrosanct - if a schema is
touched, explain the migration. -->
