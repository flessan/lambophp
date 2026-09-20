//! Does a managed PHP runtime actually run?
//!
//! Everything else in the runtime tests reasons about files: digests,
//! manifests, directory layouts. A runtime can satisfy all of them and still
//! not execute - wrong platform, missing library, an archive that verified
//! perfectly and contained the wrong thing. This file closes that gap by
//! starting PHP and asserting on what it reported.
//!
//! # What is and is not being proven here
//!
//! The `php` in these archives is `tests/fixtures/runtime/src/php`: a shell
//! script that implements the command surface Lambo drives and nothing else.
//! So these tests prove that **Lambo's** discovery, `PHPRC` wiring, health
//! checking and script execution are correct. They do **not** prove anything
//! about PHP itself - a real interpreter could disagree with the fixture.
//!
//! Proving things about real PHP is what `tests/real_artifacts.rs` is for. It
//! skips loudly unless a genuine PHP archive is staged, so a green run here
//! must never be read as "verified against PHP".
//!
//! Every test is `#[cfg(unix)]`: the fixture is a `#!/bin/sh` script, so it
//! cannot execute on Windows. Windows runtime execution is covered by the
//! manual smoke test in `docs/windows.md`, not by CI, and this file does not
//! pretend otherwise.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use lambo_core::catalog::{Catalog, Family, Release};
use lambo_core::download::LocalDownloader;
use lambo_core::error::Error;
use lambo_core::paths::Paths;
use lambo_core::php::{self, RuntimeHealth};
use lambo_core::platform::Os;
use lambo_core::runtime::{self, InstalledRuntime, RuntimeKind};

/// The directory holding the committed artifacts.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime")
}

/// A temporary Lambo home that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-exec-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("failed to create the temporary directory");
        Self { path }
    }

    fn home(&self) -> Paths {
        Paths::from_root(&self.path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The fixture catalogue with `fixture://` rewritten to a local file URL.
fn fixture_catalog() -> Catalog {
    let text = std::fs::read_to_string(fixtures().join("catalogue.json"))
        .expect("the fixture catalogue must be present");
    let catalog: Catalog = serde_json::from_str(&text).expect("the fixture catalogue must parse");

    let base = fixtures();
    let rewritten = catalog
        .releases(Family::Php)
        .iter()
        .map(|release| {
            let file_name = release
                .url
                .strip_prefix("fixture://")
                .expect("every fixture URL uses the fixture scheme");
            Release {
                url: format!("file://{}", base.join(file_name).display()),
                ..release.clone()
            }
        })
        .collect::<Vec<_>>();

    Catalog {
        php: rewritten,
        ..catalog
    }
}

/// The fixture release that works.
fn healthy_release() -> Release {
    find_release("8.4.2", "linux-x64")
}

/// The fixture release whose archive is intact but whose binary will not run.
fn broken_release() -> Release {
    find_release("8.4.3", "linux-x64")
}

fn find_release(version: &str, platform: &str) -> Release {
    fixture_catalog()
        .releases(Family::Php)
        .iter()
        .find(|r| r.version == version && r.platform == platform)
        .cloned()
        .unwrap_or_else(|| panic!("the fixture catalogue has a {version} {platform} entry"))
}

/// Installs the working fixture and returns it.
fn install_healthy(paths: &Paths) -> InstalledRuntime {
    php::install_release(
        paths,
        &healthy_release(),
        &LocalDownloader,
        &Default::default(),
    )
    .expect("the healthy fixture must install")
}

// ---------------------------------------------------------------------------
// A runtime that works
// ---------------------------------------------------------------------------

#[test]
fn an_installed_runtime_runs_and_reports_the_version_lambo_installed() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    assert!(health.ran, "the fixture should run: {:?}", health.problem);
    assert!(health.is_healthy(), "{:?}", health.problem);
    // The point of the check: this is what the binary itself said, not what
    // the catalogue claimed. A runtime that reports nothing is not verified.
    assert_eq!(
        health.reported_version.map(|v| v.to_string()),
        Some("8.4.2".to_owned()),
        "the reported version must come from PHP, not from the catalogue"
    );
    assert_eq!(health.executable, Some(installed.path.join("bin/php")));
}

#[test]
fn the_generated_php_ini_is_the_one_php_actually_loads() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let expected = php::php_ini_path(&installed, Os::Linux);
    assert!(
        expected.is_file(),
        "install must generate {}",
        expected.display()
    );

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    // This is the assertion the whole "users never edit php.ini" promise rests
    // on. If PHPRC did not reach the child, PHP would report no configuration
    // file and every setting Lambo wrote would be inert.
    assert_eq!(
        health.loaded_ini.as_deref(),
        Some(expected.as_path()),
        "PHP must load the ini Lambo generated"
    );
}

#[test]
fn the_module_list_proves_phprc_reached_the_child_process() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    // The fixture derives its module list from whatever php.ini it was handed,
    // so a non-empty list that matches the generated ini is evidence the
    // environment variable arrived - not just that `php -m` parsed.
    let ini = php::php_ini_path(&installed, Os::Linux);
    let enabled: Vec<String> = std::fs::read_to_string(&ini)
        .expect("the generated ini must be readable")
        .lines()
        .filter_map(|line| line.strip_prefix("extension="))
        .map(str::to_owned)
        .collect();
    assert!(
        !enabled.is_empty(),
        "the fixture ships modules, so some must be enabled"
    );

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    for extension in &enabled {
        assert!(
            health.modules.iter().any(|module| module == extension),
            "{extension} was enabled in the generated ini but PHP did not report it; \
             reported {:?}",
            health.modules
        );
    }
    // Section headers are not modules.
    assert!(
        !health.modules.iter().any(|m| m.starts_with('[')),
        "php -m section headers must be filtered out"
    );
}

// ---------------------------------------------------------------------------
// Runtimes that do not work
// ---------------------------------------------------------------------------

#[test]
fn a_runtime_that_will_not_start_is_reported_broken_not_installed() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    // Break it after the fact: the same failure an intact-but-unrunnable
    // archive produces, reached by deleting what the loader needs.
    std::fs::write(installed.path.join("bin/.fixture-fail"), "127").unwrap();

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    assert!(
        !health.ran,
        "a runtime that cannot start must not read as run"
    );
    assert!(!health.is_healthy());
    let problem = health.problem.expect("the reason must be recorded");
    // The reason has to carry what PHP printed. "PHP is broken" is not a
    // diagnosis; the loader's message is.
    assert!(
        problem.contains("libfixture.so.1"),
        "the diagnosis must quote what PHP said, got: {problem}"
    );
    assert!(problem.contains("127"), "and the exit code: {problem}");
}

#[test]
fn a_runtime_with_no_executable_reports_none_rather_than_a_directory() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    std::fs::rename(
        installed.path.join("bin/php"),
        installed.path.join("bin/php.moved"),
    )
    .unwrap();

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    assert!(!health.ran);
    // The point of the assertion. This field used to fall back to the runtime
    // directory, so the CLI printed a directory in a column headed
    // "executable" - a false statement about a file that does not exist.
    assert_eq!(
        health.executable, None,
        "a missing executable must be reported as missing, not as a path"
    );
    let problem = health.problem.expect("the reason must be recorded");
    assert!(problem.contains("no PHP executable was found"), "{problem}");
}

#[test]
fn orchestrating_a_project_refuses_a_runtime_that_will_not_run() {
    use lambo_core::config::{Config, DatabaseKind, ServerKind};
    use lambo_core::platform::{Arch, Platform};
    use lambo_core::project::Project;
    use lambo_core::session::{self, Context};

    let temp = TempDir::new();
    let paths = temp.home();
    paths.ensure_layout().expect("layout");
    let installed = install_healthy(&paths);

    // A plain-PHP project, the shape `lambo init` writes for one.
    let root = temp.path.join("shop");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();
    std::fs::write(
        root.join("lambo.yml"),
        "server:\n  kind: php\n  document_root: .\ndatabase:\n  kind: none\n",
    )
    .unwrap();
    let project = Project::load(&root).expect("project loads");

    let mut config = Config::default();
    config.server.kind = ServerKind::Php;
    config.database.kind = DatabaseKind::None;

    // Healthy first: the gate must not reject a runtime that works.
    let context = Context {
        paths: paths.clone(),
        config: config.clone(),
        catalog: lambo_core::catalog::Catalog::embedded().expect("catalogue"),
        platform: Platform::new(Os::host(), Arch::X86_64),
        downloader: &LocalDownloader,
        os: Os::host(),
        log: lambo_core::logs::nop_log(),
    };
    let mut healthy = context;
    session::ensure_php(&project, &mut healthy).expect("a working runtime must be accepted");

    // Now break it the way a missing shared library would. It still resolves:
    // the directory is there, the manifest is intact, the executable exists.
    std::fs::write(installed.path.join("bin/.fixture-fail"), "127").unwrap();

    let mut broken = Context {
        paths: paths.clone(),
        config,
        catalog: lambo_core::catalog::Catalog::embedded().expect("catalogue"),
        platform: Platform::new(Os::host(), Arch::X86_64),
        downloader: &LocalDownloader,
        os: Os::host(),
        log: lambo_core::logs::nop_log(),
    };
    let error = session::ensure_php(&project, &mut broken)
        .expect_err("`lambo up` must not proceed on a runtime that cannot run");

    // The point: without this check, `lambo up` would start a web server, see
    // its port answer, and report the project as serving while PHP could not
    // execute a single request.
    match &error {
        Error::ServiceFailed { reason, hint, .. } => {
            assert!(
                reason.contains("libfixture.so.1"),
                "the reason must carry what PHP said: {reason}"
            );
            assert!(
                hint.as_deref().unwrap_or_default().contains("lambo php"),
                "and name the command that fixes it: {hint:?}"
            );
        }
        other => panic!("expected ServiceFailed, got {other}"),
    }
}

#[test]
fn doctor_does_not_tick_a_project_runtime_it_could_not_start() {
    use lambo_core::config::Config;
    use lambo_core::doctor::{self, Severity};
    use lambo_core::project::Project;

    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let root = temp.path.join("shop");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();
    std::fs::write(
        root.join("lambo.yml"),
        "php: '=8.4.2'\nserver:\n  kind: php\n  document_root: .\ndatabase:\n  kind: none\n",
    )
    .unwrap();
    let project = Project::load(&root).expect("project loads");
    let config = Config::default();

    let php_check = || -> doctor::Check {
        doctor::check_project_runtime(&project, &config, &paths, Os::host())
            .into_iter()
            .find(|check| check.name == "project php")
            .expect("the project php check runs")
    };

    // Healthy: a tick, and the tick is earned by having started the binary.
    let healthy = php_check();
    assert!(matches!(healthy.severity, Severity::Ok), "{healthy:?}");

    // Break it. It still resolves - the directory, the manifest and the
    // executable are all present - so a check that only asked "is a satisfying
    // version installed" would still tick. That tick is the bug: it is a claim
    // about serving requests made without ever serving one.
    std::fs::write(installed.path.join("bin/.fixture-fail"), "127").unwrap();

    let broken = php_check();
    assert!(
        matches!(broken.severity, Severity::Fail),
        "a runtime that will not start must not read as satisfying the project: {broken:?}"
    );
    assert!(
        broken.detail.contains("libfixture.so.1"),
        "and must carry what PHP said: {}",
        broken.detail
    );
    assert!(
        broken
            .fix
            .as_deref()
            .unwrap_or_default()
            .starts_with("lambo php"),
        "and name the command that fixes it: {:?}",
        broken.fix
    );
}

#[test]
fn a_runtime_reporting_a_different_version_is_flagged_but_still_runs() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    std::fs::write(installed.path.join("bin/.fixture-version"), "8.1.0").unwrap();

    let health = RuntimeHealth::check(&paths, &installed, Os::Linux);

    // It runs, so it is usable - but the archive and the catalogue entry
    // disagree, and hiding that would be hiding a real bug in one of them.
    assert!(health.ran);
    assert!(!health.is_healthy(), "a version disagreement is a problem");
    let problem = health.problem.expect("the disagreement must be reported");
    assert!(problem.contains("8.1.0"), "{problem}");
    assert!(problem.contains("8.4.2"), "{problem}");
}

#[test]
fn installing_an_archive_that_will_not_run_installs_nothing() {
    let temp = TempDir::new();
    let paths = temp.home();

    // The digest matches and the archive unpacks cleanly. Every check up to
    // "start the binary" passes, which is exactly why the last one matters.
    let error = php::install_release(
        &paths,
        &broken_release(),
        &LocalDownloader,
        &Default::default(),
    )
    .expect_err("a runtime that will not run must not be installed");

    match &error {
        Error::ServiceFailed {
            service,
            reason,
            causes,
            ..
        } => {
            assert!(service.contains("8.4.3"), "{service}");
            assert!(
                reason.contains("libfixture.so.1"),
                "the failure must say why: {reason}"
            );
            assert!(
                causes
                    .iter()
                    .any(|cause| cause.contains("verified against its digest")),
                "the user must be told the download was fine, so they look elsewhere: {causes:?}"
            );
        }
        other => panic!("expected ServiceFailed, got {other}"),
    }

    assert!(
        runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty(),
        "a runtime that will not run must not be left behind looking installed"
    );
    assert!(
        !paths
            .runtime_version_dir(RuntimeKind::Php, "8.4.3")
            .exists(),
        "and its directory must be removed"
    );
}

// ---------------------------------------------------------------------------
// Running PHP through Lambo
// ---------------------------------------------------------------------------

#[test]
fn a_script_runs_through_the_managed_runtime() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let script = temp.path.join("index.php");
    std::fs::write(&script, "<?php echo 'hello';\n").unwrap();

    let run = php::probe(
        &paths,
        &installed,
        &[script.display().to_string().as_str()],
        Os::Linux,
    )
    .expect("probe");

    assert!(run.success, "exit {:?}: {}", run.code, run.combined());
    assert!(
        run.stdout.contains("index.php"),
        "the script Lambo was given must be the one that ran: {}",
        run.stdout
    );
}

#[test]
fn a_missing_script_reports_the_failure_rather_than_swallowing_it() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let run = php::probe(&paths, &installed, &["/does/not/exist.php"], Os::Linux)
        .expect("probe itself succeeds; PHP is what fails");

    assert!(!run.success, "a missing script must not read as success");
    assert_eq!(run.code, Some(2), "PHP exits 2 for an unreadable file");
    assert!(
        run.combined().contains("Could not open input file"),
        "and must say why: {}",
        run.combined()
    );
}

#[test]
fn serve_spec_and_run_agree_on_how_phprc_is_passed() {
    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let serve = php::serve_spec(
        &installed,
        8080,
        &temp.path,
        &temp.path.join("server.log"),
        Os::Linux,
    )
    .expect("serve_spec");

    let phprc = serve
        .env
        .get("PHPRC")
        .expect("the built-in server must be given the generated configuration");

    // PHP accepts a directory or a file here, so both call sites used to work
    // while disagreeing. They must not drift apart again: one contract, one
    // helper.
    let ini = php::php_ini_path(&installed, Os::Linux);
    let expected = ini.parent().expect("php.ini has a parent directory");
    assert_eq!(
        Path::new(phprc),
        expected,
        "PHPRC must be the directory holding php.ini, matching php::run"
    );
}

#[test]
fn the_database_manager_is_served_with_the_generated_configuration() {
    use lambo_core::dbui::{self, DbUi};

    let temp = TempDir::new();
    let paths = temp.home();
    let installed = install_healthy(&paths);

    let entry = lambo_core::dbui::entry_path(&paths);
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, "<?php // phpMyAdmin\n").unwrap();

    let ui = DbUi {
        entry,
        php: installed.clone(),
    };
    let plan =
        dbui::Plan::new(&paths, &ui, 8081, 3306, "root", Os::Linux, "phpmyadmin").expect("plan");

    let spec = dbui::command_spec(&plan);
    let phprc = spec.env.get("PHPRC").unwrap_or_else(|| {
        panic!(
            "Adminer is a PHP application and must get the generated configuration; env was {:?}",
            spec.env
        )
    });

    // Why this matters: the extensions Adminer needs to reach a database are
    // enabled in Lambo's php.ini. Served without it, `lambo db open` shows a
    // login form that cannot connect to anything.
    let expected = php::php_ini_path(&installed, Os::Linux);
    assert_eq!(
        Path::new(phprc),
        expected.parent().expect("php.ini has a parent"),
        "the manager must be served with the same configuration as everything else"
    );
    assert!(
        expected.is_file(),
        "and that configuration must exist at {}",
        expected.display()
    );
}

// The output parsers (`php -v`, `php --ini`, `php -m`) are pinned to real PHP
// banner shapes by unit tests in `src/php.rs`. They are pure functions over
// text, so testing them there keeps them out of the public API; what belongs
// in this file is the part that needs a process.
