# Release process

## Artifacts

Every release publishes these, built on the platform they run on:

| File | Platform |
| --- | --- |
| `lambo-windows-amd64.exe` · `LamboPHP-windows-amd64.zip` · `LamboPHP-Setup.exe` | Windows 10/11 x86_64 |
| `lambo-linux-amd64` · `LamboPHP-linux-amd64.tar.gz` | Linux x86_64 |
| `lambo-linux-arm64` · `LamboPHP-linux-arm64.tar.gz` | Linux ARM64 |
| `lambo-macos-amd64` · `LamboPHP-macos-amd64.tar.gz` | macOS Intel |
| `lambo-macos-arm64` · `LamboPHP-macos-arm64.tar.gz` | macOS Apple silicon |
| `SHA256SUMS` | every artifact above |

Nothing is cross-compiled. An installer in particular has to be built on the
platform it installs to, and it is the artifact users are most entitled to
trust.

## Cutting a release

1. **Verify the gaps are still the gaps.** Read
   [roadmap.md](roadmap.md#next-in-priority-order),
   and say plainly in the release notes what does not work yet. Today that is
   the catalogue entries no publisher digest exists for (the static-php PHP
   builds, Oracle MySQL, Adminer - see
   [roadmap.md](roadmap.md#1-pinned-checksums-for-every-catalogue-entry)), no
   macOS server builds, no TLS, and an application window nobody has watched
   run.

2. **Run the whole suite locally**, on the platform you have:

   ```bash
   cargo test --workspace --all-targets
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all --check
   cargo check --workspace --all-targets --target x86_64-pc-windows-msvc
   ```

3. **Run the manual Windows smoke test** in
   [windows.md](windows.md#manual-smoke-test). CI cannot download real runtimes
   or start real servers; this checklist is the only thing that proves the
   acceptance path works. Record the result in the release notes - including
   the steps you could not run.

4. **Bump the version** in the workspace `Cargo.toml`, refresh
   `CHANGELOG.md` (move the unreleased notes into a dated section), and commit.

5. **Tag and push:**

   ```bash
   git tag -s v0.3.0 -m "Lambo PHP 0.3.0"
   git push origin main --tags
   ```

   The `Release` workflow in the published repository builds all five platforms,
   packages them, checksums them, verifies the manifest covers every file, and
   publishes the GitHub release.

   **Where that workflow lives, and what this checkout can verify.** This work
   tree carries `.github/workflows/ci.yml` and nothing else: the release
   pipeline is part of the published repository, so steps 1-4 above are all it
   can run. The packaging itself is here and is the same code the pipeline
   calls - `scripts/package.sh`, `scripts/build-windows.ps1` and
   `scripts/installer.ps1` - and `docs/windows.md`'s checklist covers what a
   release candidate has to pass on a real machine. Do not read a green local
   run as a published release.

   A workflow file also cannot be edited from a session whose token lacks the
   App's `workflows` permission: the push is refused with `refusing to allow a
   GitHub App to create or update workflow ... without workflows permission`,
   which is a permissions fact rather than a content one. Whoever holds that
   permission applies those one-line changes.

6. **Check the release.** Download `SHA256SUMS` and one archive, verify, run
   `lambo --version` and `lambo doctor` from the extracted binary. For Windows,
   also run step 1 of [windows.md](windows.md#if-nothing-starts-at-all): compare
   the installed executables against the release hashes.

## Building an artifact by hand

Useful for a hotfix, or to reproduce what CI produced.

```bash
cargo build --release --target x86_64-pc-windows-msvc
LAMBO_TARGET=x86_64-pc-windows-msvc LAMBO_NAME=windows-amd64 \
  LAMBO_ARCHIVE=zip ./scripts/package.sh
```

```powershell
pwsh ./scripts/installer.ps1        # needs NSIS: winget install NSIS.NSIS
```

`scripts/package.sh` is the same script CI runs, so a hand-built artifact has
the same layout. It writes:

```
dist/lambo-<name>[.exe]          the bare binary
dist/LamboPHP-<name>.<ext>       the archive (binary + licences + README.txt)
```

## What the installer does

`packaging/lambo.nsi`, driven by `scripts/installer.ps1`:

- installs per-user into `%LOCALAPPDATA%\Programs\LamboPHP` - no elevation;
- adds that directory to the **user** `PATH`, never the machine one;
- creates Start Menu shortcuts and an uninstaller;
- offers to run `lambo doctor` on the last page;
- on uninstall, removes the binary, the shortcuts, and its own `PATH` entry -
  and leaves `%USERPROFILE%\Lambo` (runtimes, databases, projects) untouched.

## Release gate

```bash
lambo doctor --release
```

Answers the one question that decides whether a catalogue is shippable: *are all
production artifacts installable and verified?*

It is `lambo doctor` plus the rules that only matter when publishing. The
important difference is one rule: a missing digest is legitimate for a running
Lambo - verification metadata is unavailable and the download fails closed - but
not in a published catalogue, where it means nobody can install that entry at
all. It also rejects:

- a declared `executable` that is not a relative path inside the archive
- a `size` declared as zero
- everything `lambo doctor` already rejects: duplicate identity, an invalid
  version or platform, an unusable URL, a format Lambo cannot unpack, a
  malformed digest

It validates metadata only and never downloads an artifact, so it is cheap
enough to run on every commit as well as before tagging. Exit code is non-zero
when anything blocks.

Against the current catalogue it reports **0 development-blocking issues and 13
release-blocking ones** - the entries whose publishers expose no usable digest
(the static-php PHP builds, Oracle MySQL, Adminer and the two older Windows PHP
archives, which still verify through their upstream sidecars at run time where
those exist). The release gate is stricter than the run-time rule by design: a
release should not depend on a file Lambo does not control.

## Catalogue checksums

A release that ships a catalogue with unpinned checksums is a release where
`lambo php install` fails closed. `lambo doctor --release` refuses to pass until
every entry has a real `sha256`.

### How a maintainer pins one

```bash
# 1. download the artifact from its official source
curl -LO https://…/php-8.4.2-cli-linux-x86_64.tar.gz

# 2. compute the digest
lambo config hash php-8.4.2-cli-linux-x86_64.tar.gz

# 3. check that value against the publisher's own published checksum
```

Step 3 is the one that matters. A digest computed from the file you are about to
ship only proves the file equals itself; the point is to confirm it equals what
upstream says it is. `lambo config hash` therefore prints and stops - it never
writes the value anywhere, because recording the digest of whatever it was
handed would turn every install into trust-on-first-use.

Then record it in `crates/lambo-core/catalogs/default.json` and re-run the gate.

A CI job should also verify each pinned digest still matches upstream; a stale
digest is a bug users hit as a "download failed" error.

## Signing

Binaries and the installer are **not** code-signed yet, so Windows SmartScreen
warns. That is documented in [windows.md](windows.md#antivirus-and-smartscreen)
rather than hidden. Adding a signing certificate is tracked in the roadmap.
