# Real artifact testing

Everything else in this suite runs against small fixture archives that describe a
PHP which does not exist. That proves Lambo's *behaviour* - resolution,
verification, extraction, transactional install - but it cannot prove that a real
upstream archive installs correctly, because the fixtures are not real archives.

This directory closes that gap, without committing large binaries.

## How it works

A test (`crates/lambo-core/tests/real_artifacts.rs`) looks here for manifest
files. If it finds none it passes immediately and says so, so normal CI never
depends on anything in this directory. If you place a manifest and the archive it
names, the test runs a full end-to-end install against the real thing.

Nothing here is read by Lambo itself. It is test-only, and the production
catalogue in `crates/lambo-core/catalogs/default.json` is never modified by it.

## Running a real artifact

1. Download the artifact from its official source.

2. Calculate its digest, and check that value against the publisher's own
   published checksum before you trust it:

   ```bash
   lambo config hash /path/to/php-8.4.2-cli-linux-x86_64.tar.gz
   ```

   This is the step that makes the test meaningful. A digest you computed from
   the file you are about to test only proves the file equals itself; the point
   is to confirm it equals what upstream says it is.

3. Copy `manifest.example.json` to `<something>.json` in this directory and fill
   it in.

4. Put the archive beside the manifest, with the file name the manifest names.

5. Run:

   ```bash
   cargo test -p lambo-core --test real_artifacts -- --nocapture
   ```

## What the test checks

For each manifest it runs the real install pipeline and asserts that:

- the artifact verifies against the digest you recorded;
- it extracts to the layout Lambo expects;
- the declared primary executable is present in the result;
- the installed runtime passes an integrity check;
- the install manifest records the source and the digest actually used.

A digest mismatch, or an archive whose layout does not contain the declared
executable, fails the test. That is the signal that a catalogue entry describes
a different artifact than the one it points at.

### PHP artifacts additionally prove the runtime runs

For a `php` manifest the test goes further, because a real interpreter is the
only thing that can confirm Lambo reads real PHP output correctly:

- the install itself started the binary - `install_release` refuses to install a
  runtime that will not run, so a passing install means real PHP executed;
- `php -v` reported the same version the manifest declares. A mismatch means the
  pinned entry describes a different build than the archive it points at;
- `php --ini` reported Lambo's generated `php.ini` as the loaded configuration.
  This is the "users never edit php.ini" promise checked against a real
  interpreter rather than against a stand-in;
- `php -m` reported the always-compiled-in modules (`Core`, `date`, `standard`).
  An empty list here means the module parser stopped matching real output.

The rest of the suite runs against a fixture `php` that implements the command
surface Lambo drives and nothing else. That proves Lambo's plumbing; only this
test proves the plumbing agrees with real PHP. **A green fixture run is not
verification against a real PHP build**, and the two are never conflated in the
output.

These checks need an artifact built for the machine running the test. A Windows
archive supplied on Linux installs but is not started, so the health assertions
do not apply to it.

## Keeping binaries out of the repository

Everything in this directory except `README.md` and `manifest.example.json` is
git-ignored. Do not force-add an archive: runtime archives are tens of
megabytes, and a repository that carries them is slow for everybody forever.
