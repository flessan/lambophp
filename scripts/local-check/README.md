# Local verification without a registry

CI is the authority: it runs `cargo test --workspace --all-targets --locked`,
`cargo clippy -D warnings` and `cargo fmt --all --check` on Windows, Linux and
macOS. Nothing in this directory replaces that.

Some of these exist because the crate registry cannot be reached and no Rust
toolchain is installed (a sandbox with no proxy, an air-gapped machine); each
makes a *part* of CI reproducible there, so that an obvious mistake is caught
before a push instead of after. The last two do not touch the toolchain at all -
they read the tree - and they guard things a green build would never notice.

| Script | What it does | What it proves |
| --- | --- | --- |
| `fetch-toolchain.sh` | Installs `rustfmt`, `rustc`, `rust-std` and `cargo` for the pinned toolchain from the crates that vendors publish on the npm registry, and assembles a `--sysroot` | `rustfmt` behaves exactly as CI's does; `rustc` can compile anything that needs no external crate |
| `check-format.sh` | Runs the fetched `rustfmt` over every `.rs` file in `crates/` | `cargo fmt --all --check` has nothing to report |
| `generate_harness.py` | Builds a rustc-only crate root around the real `lambo-core` sources - real files wherever possible, hand-written `Display` for `Error` instead of `thiserror`, small stubs where a module needs `serde`, `flate2` or `crc32fast` | The harnessed modules **compile**, and their **unit tests run and pass**, on the machine that wrote them |
| `check-ci-filter.py` | Compares the `windows-behaviour` job's test filter in `ci.yml` with `windows-test-modules.txt` | The job runs the tests it says it runs - a filter that matches less than it should still exits 0, so nothing else would ever notice |

None of these is part of the product, none is built by Cargo, and none is run by
CI. They exist because "it compiles" and "the audit is complete" are claims that
should be checkable before a push.

`check-ci-filter.py` is the one that guards something CI cannot: the job's filter
selects tests by *substring*, so a module added or renamed after the filter was
written stops being covered without a single red mark. When this directory was
first written the job selected 167 of the 250 tests it was meant to run. Add any
module with tests to `windows-test-modules.txt` and the script will require the
filter to select it.

`generate_harness.py` prints what it stubbed. A module that is stubbed out is
*not* verified by it, and a Windows-only module is not verified on Linux at all:
`cargo check --target x86_64-pc-windows-msvc` on the Windows CI runner is the
only thing that compiles those.

## How to run it

```sh
./scripts/local-check/fetch-toolchain.sh          # installs the pinned toolchain
./scripts/local-check/check-format.sh             # cargo fmt --all --check, offline
python3 scripts/local-check/check-ci-filter.py    # the CI job's test coverage
python3 scripts/local-check/generate_harness.py   # writes scripts/local-check/harness/
export LD_LIBRARY_PATH=/tmp/lambo-toolchain/sysroot/lib
CARGO_PKG_VERSION=0.14.0-rc.1 \
CARGO_PKG_REPOSITORY=https://github.com/flessan/kink-php-dev \
  /tmp/lambo-toolchain/sysroot/bin/rustc --edition 2024 --test \
  --sysroot /tmp/lambo-toolchain/sysroot -o /tmp/harness-test \
  scripts/local-check/harness/lib.rs
/tmp/harness-test
```

`CARGO_PKG_VERSION` and `CARGO_PKG_REPOSITORY` stand in for what Cargo would
set; without them `env!` fails to compile. The harness directory is generated,
and safe to delete.

## What it has found

The harness is not decorative: it caught a real parse error (`download_cache.rs`
ended a string with a hex escape), a missing `nop_progress` import, `self.log(…)`
called on a function-pointer field, `const fn` setters missing from a `static`
catalogue, and a `Component` method that collided with a builder setter. Two of
its failures were *harness* bugs - the ZIP reader in the stub read the entry
count from the wrong offset, and its own fixtures lacked an executable bit - and
the rest were tests whose fixtures had drifted from the code (an
`AllowOverride` block that the patch had already rewritten, a vhost fixture left
on the dashboard's Apache default while the test expected nginx files).

## What it does not prove

* Anything that needs a crate: `serde`, `flate2`, `crc32fast`, `thiserror`,
  `semver`, `windows-sys`. Those modules are stubbed or transcribed.
* Anything Windows-only: no `windows-sys`, no registry, no `ShellExecuteW`, no
  service control. `lambo-process-windows` and `lambo-gui` are outside it
  entirely.
* Clippy. `cargo clippy -D warnings` is CI's, and nothing here imitates it.
* That a Rust name *behaves* like the function it replaced. The test suites
  are what carry behaviour.
