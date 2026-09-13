//! `lambo php` driven end to end, against a runtime that really executes.
//!
//! `cli_lifecycle.rs` proves the service lifecycle. This file proves the other
//! half of the acceptance path: that `lambo php install` produces a runtime
//! that runs, and that the commands reporting on it report what PHP actually
//! said rather than what the catalogue claimed.
//!
//! Everything here is the real `lambo` binary, the real install pipeline
//! (download → checksum → extract → verify by running) and a real child
//! process. Only the interpreter is a stand-in.
//!
//! # What the stand-in does and does not prove
//!
//! The installed `php` is `lambo-core`'s fixture: a shell script implementing
//! the command surface Lambo drives. So a green run here means Lambo's install,
//! discovery and health reporting are correct. It does **not** mean a real PHP
//! build behaves this way - that is `lambo-core`'s `real_artifacts` test, which
//! skips loudly unless a genuine artifact is staged.
//!
//! Unix only: the fixture is a `#!/bin/sh` script. Windows runtime execution is
//! a manual smoke test (see `docs/windows.md`), and this file does not claim to
//! cover it.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lambo_core::sha256::sha256_file;

/// The `lambo` binary under test.
fn lambo() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lambo"))
}

/// The committed fixture artifacts, in the neighbouring crate.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../lambo-core/tests/fixtures/runtime")
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "lambo-cli-php-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A Lambo home with a catalogue override pointing at the fixture archives.
///
/// This is the same mechanism a user has for installing from their own mirror,
/// so the test goes through the production code path rather than a test hook.
struct Harness {
    home: TempDir,
}

impl Harness {
    fn new() -> Self {
        let home = TempDir::new("home");
        let catalogs = home.path.join("config/catalogs");
        fs::create_dir_all(&catalogs).expect("catalogs directory");

        // Digests are computed from the files rather than copied, so a fixture
        // rebuilt without updating this test fails here instead of installing
        // something the test did not mean to.
        //
        // Written by hand rather than through serde_json, which is not a
        // dependency of this crate; adding one for a test fixture is not worth
        // a lockfile change.
        let entry = |version: &str, archive: &str| -> String {
            let path = fixtures().join(archive);
            let digest = sha256_file(&path).expect("the fixture archive must be readable");
            format!(
                r#"    {{
      "version": "{version}",
      "platform": "linux-x64",
      "url": "file://{url}",
      "sha256": "{digest}",
      "archive_format": "tar.gz",
      "filename": "{archive}",
      "channel": "stable",
      "executable": "bin/php"
    }}"#,
                url = path.display()
            )
        };

        let catalog = format!(
            "{{\n  \"schema\": 1,\n  \"php\": [\n{},\n{}\n  ]\n}}\n",
            entry("8.4.2", "php-8.4.2-linux-x64.tar.gz"),
            // Verifies cleanly, refuses to run.
            entry("8.4.3", "php-8.4.3-linux-x64.tar.gz"),
        );

        fs::write(catalogs.join("php.json"), catalog).expect("catalogue override written");

        Self { home }
    }

    fn lambo(&self, args: &[&str]) -> Output {
        Command::new(lambo())
            .args(args)
            .current_dir(&self.home.path)
            .env(lambo_core::paths::HOME_ENV, &self.home.path)
            .env("NO_COLOR", "1")
            .output()
            .unwrap_or_else(|error| panic!("could not run `lambo {}`: {error}", args.join(" ")))
    }

    fn expect_success(&self, args: &[&str]) -> String {
        let output = self.lambo(args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "`lambo {}` exited with {:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            args.join(" "),
            output.status.code()
        );
        stdout
    }

    fn expect_failure(&self, args: &[&str]) -> String {
        let output = self.lambo(args);
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "`lambo {}` was expected to fail but exited zero\n{combined}",
            args.join(" ")
        );
        combined
    }
}

#[test]
fn install_then_current_reports_what_php_itself_said() {
    let harness = Harness::new();

    harness.expect_success(&["php", "install", "8.4.2"]);
    let current = harness.expect_success(&["php", "current"]);

    // `version` is what Lambo installed. `reported` is what the binary said
    // when Lambo started it. Printing both is the point: a runtime that will
    // not run shows up as a gap between them rather than looking installed.
    assert!(current.contains("8.4.2"), "{current}");
    assert!(
        current.contains("reported"),
        "`lambo php current` must show what PHP reported, not just the catalogue version:\n{current}"
    );
    assert!(
        current.contains("configuration"),
        "and which configuration file PHP loaded:\n{current}"
    );
    assert!(
        current.contains("extensions"),
        "and how many extensions loaded:\n{current}"
    );
}

#[test]
fn modules_lists_what_the_installed_runtime_reports() {
    let harness = Harness::new();
    harness.expect_success(&["php", "install", "8.4.2"]);

    let modules = harness.expect_success(&["php", "modules"]);

    // The fixture derives its module list from the php.ini it was handed, so
    // these names appearing is evidence the generated configuration reached
    // the child process.
    for expected in ["Core", "openssl"] {
        assert!(
            modules.lines().any(|line| line.trim() == expected),
            "`lambo php modules` should list {expected}:\n{modules}"
        );
    }
}

#[test]
fn installing_a_runtime_that_will_not_run_fails_with_the_reason() {
    let harness = Harness::new();

    let output = harness.expect_failure(&["php", "install", "8.4.3"]);

    // The user must be told the download was fine, or they will spend the
    // evening re-downloading an archive that was never the problem.
    assert!(
        output.contains("libfixture.so.1"),
        "the failure must carry what PHP printed:\n{output}"
    );
    assert!(
        output.contains("digest"),
        "and must say the archive verified, so the user looks elsewhere:\n{output}"
    );

    // Nothing may be left behind looking installed. Asserting on the STATUS
    // column rather than substring-matching the line: the PATH column reads
    // "not installed" for every absent version, so a naive contains("installed")
    // passes for the wrong reason.
    let list = harness.expect_success(&["php", "list"]);
    let status_of = |version: &str| -> Option<String> {
        list.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let row_version = fields.next()?;
            let status = fields.next()?;
            (row_version == version).then(|| status.to_owned())
        })
    };
    assert_eq!(
        status_of("8.4.3").as_deref(),
        Some("available"),
        "a runtime that will not run must be offered again, not left installed:\n{list}"
    );
}

#[test]
fn a_broken_active_runtime_is_reported_as_broken_not_healthy() {
    let harness = Harness::new();
    harness.expect_success(&["php", "install", "8.4.2"]);

    // Break the installed runtime the way a missing shared library would.
    let control = harness.home.path.join("php/8.4.2/bin/.fixture-fail");
    fs::write(&control, "127").expect("control file");

    let output = harness.expect_failure(&["php", "current"]);
    assert!(
        output.contains("libfixture.so.1"),
        "`lambo php current` must say why PHP is unusable:\n{output}"
    );

    let modules = harness.expect_failure(&["php", "modules"]);
    assert!(
        !modules.contains("it may be broken"),
        "the old guess is gone; the reason is known now:\n{modules}"
    );
}

#[test]
fn doctor_names_the_runtime_it_could_not_start() {
    let harness = Harness::new();
    harness.expect_success(&["php", "install", "8.4.2"]);

    let control = harness.home.path.join("php/8.4.2/bin/.fixture-fail");
    fs::write(&control, "127").expect("control file");

    // `lambo doctor` exits non-zero for plenty of reasons in a bare home, so
    // the assertion is on what it says about PHP rather than on its status.
    let output = harness.lambo(&["doctor"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("libfixture.so.1"),
        "`lambo doctor` must quote the runtime's own failure:\n{text}"
    );
}

#[test]
fn passthrough_runs_the_managed_php() {
    let harness = Harness::new();
    harness.expect_success(&["php", "install", "8.4.2"]);

    // `lambo php -v` reaches PHP through clap's external_subcommand, which is
    // easy to break without noticing because the CLI still exits zero.
    let output = harness.expect_success(&["php", "-v"]);
    assert!(
        output.contains("PHP 8.4.2"),
        "`lambo php -v` must run the managed runtime:\n{output}"
    );
}
