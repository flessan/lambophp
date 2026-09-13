//! The application API - the boundary a GUI consumes.
//!
//! These tests exist because the point of `lambo_core::app` is that a GUI never
//! parses CLI output. That promise is only real if the models are stable,
//! structured and complete, so what is asserted here is the contract a GUI
//! codes against: field names, state vocabulary, and that everything
//! round-trips through the serializer.
//!
//! A screen that silently loses a field because a model changed is exactly the
//! failure mode this layer was built to prevent.

use std::path::PathBuf;

use lambo_core::app::{App, RuntimeState, ServiceState};
use lambo_core::paths::Paths;
use lambo_core::platform::Os;
use lambo_core::project::Project;
use lambo_core::runtime::RuntimeKind;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-app-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A plain-PHP project on disk.
fn plain_project(temp: &TempDir) -> Project {
    let root = temp.join("shop");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();
    std::fs::write(
        root.join("lambo.yml"),
        "server:\n  kind: php\n  document_root: .\ndatabase:\n  kind: none\n",
    )
    .unwrap();
    Project::load(&root).unwrap()
}

#[test]
fn a_fresh_home_yields_a_dashboard_rather_than_an_error() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();

    let app = App::open_at(&paths).expect("opens");
    let dashboard = app.dashboard(None).expect("dashboard");

    // A fresh install has no project, no runtime and nothing running. That is a
    // valid state to render, not an error to surface.
    assert!(dashboard.project.is_none());
    assert!(dashboard.php.is_none());
    assert!(dashboard.url.is_none());
    assert!(
        dashboard.healthy,
        "nothing started is not the same as something broken"
    );
    assert!(
        !dashboard.services.is_empty(),
        "the dashboard lists the services it manages even before any run"
    );
}

#[test]
fn services_report_states_a_ui_can_act_on_not_booleans() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();

    let app = App::open_at(&paths).expect("opens");
    let dashboard = app.dashboard(None).expect("dashboard");

    for service in &dashboard.services {
        // `not_started` is the honest state for a service Lambo has never run.
        // A GUI that only had on/off would have to guess which one this was.
        assert_eq!(
            service.state,
            ServiceState::NotStarted,
            "{} should be not_started on a fresh home",
            service.name
        );
        assert!(service.pid.is_none());
    }
}

#[test]
fn diagnostics_carry_words_and_a_runnable_fix() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();

    let app = App::open_at(&paths).expect("opens");
    let diagnostics = app.doctor(None);

    assert!(!diagnostics.is_empty(), "a bare home must produce findings");

    for diagnostic in &diagnostics {
        // Words, not the terminal glyphs `Severity::marker` prints. A GUI
        // styles by name and must not have to know what a console shows.
        assert!(
            matches!(
                diagnostic.severity.as_str(),
                "ok" | "warning" | "error" | "unsupported"
            ),
            "`{}` has a severity a UI would have to guess at: {}",
            diagnostic.name,
            diagnostic.severity
        );

        if diagnostic.severity != "ok" {
            let fix = diagnostic.fix.as_deref().unwrap_or("");
            assert!(!fix.is_empty(), "`{}` fails with no fix", diagnostic.name);
            assert!(
                fix.starts_with("lambo ") || fix.starts_with('`') || fix.starts_with("install"),
                "`{}` names a fix that is not a command: {fix}",
                diagnostic.name
            );
        }
    }
}

#[test]
fn projects_round_trip_through_the_registry() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();
    let project = plain_project(&temp);
    let root = project.document_root();

    let app = App::open_at(&paths).expect("opens");
    assert!(app.projects().unwrap().is_empty());

    assert!(app.add_project(&root).unwrap(), "first add is new");
    assert!(!app.add_project(&root).unwrap(), "second add is a no-op");

    let listed = app.projects().unwrap();
    assert_eq!(listed.len(), 1);
    let info = &listed[0];
    assert_eq!(info.name, "shop");
    assert_eq!(info.path, root);
    assert_eq!(
        info.database, "none",
        "the project's own choice is honoured"
    );
    assert!(
        info.url.starts_with("http://localhost"),
        "the URL is a real URL: {}",
        info.url
    );
    assert!(!info.serving, "nothing is running yet");

    assert!(app.remove_project(&root).unwrap());
    assert!(app.projects().unwrap().is_empty());
    // Removing a project must never touch the user's files.
    assert!(root.join("index.php").is_file());
}

#[test]
fn every_model_serializes_because_that_is_the_gui_boundary() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();
    let project = plain_project(&temp);

    let app = App::open_at(&paths).expect("opens");
    app.add_project(&project.document_root()).unwrap();

    // The GUI consumes these across a serialization boundary. If a model stops
    // being serializable the GUI stops working, and a compile-time derive is
    // not enough of a guarantee on its own - this asserts the JSON actually
    // contains the fields a screen binds to.
    let dashboard = app.dashboard(Some(&project)).expect("dashboard");
    let json = serde_json::to_value(&dashboard).expect("dashboard serializes");

    for field in [
        "project",
        "services",
        "php",
        "url",
        "database_ui_url",
        "healthy",
    ] {
        assert!(
            json.get(field).is_some(),
            "the dashboard lost `{field}`; a GUI binds to it. JSON: {json}"
        );
    }

    let projects = app.projects().unwrap();
    let json = serde_json::to_value(&projects).expect("projects serialize");
    let first = &json[0];
    for field in [
        "name",
        "path",
        "framework",
        "document_root",
        "php",
        "database",
        "url",
        "serving",
    ] {
        assert!(
            first.get(field).is_some(),
            "a project lost `{field}`; a GUI binds to it. JSON: {first}"
        );
    }

    let diagnostics = app.doctor(Some(&project));
    let json = serde_json::to_value(&diagnostics).expect("diagnostics serialize");
    assert!(
        json[0].get("severity").is_some() && json[0].get("detail").is_some(),
        "a diagnostic lost a field a GUI binds to: {json}"
    );

    let logs = app.logs(10).unwrap();
    let json = serde_json::to_value(&logs).expect("logs serialize");
    assert!(
        json[0].get("lines").is_some(),
        "a log view lost its lines: {json}"
    );
}

#[test]
fn an_active_runtime_reports_the_evidence_it_runs() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();

    let app = App::open_at(&paths).expect("opens");
    assert!(
        app.active_runtime().unwrap().is_none(),
        "no runtime is an ordinary state, not an error"
    );

    // A runtime that is present but not runnable must not be reported as
    // healthy. This is the distinction the whole health check exists for.
    let root = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
    std::fs::create_dir_all(&root).unwrap();
    let exe = root.join(Os::host().executable_name("php"));
    std::fs::write(&exe, "#!/bin/sh\nexit 127\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    lambo_core::runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();

    let app = App::open_at(&paths).expect("opens");
    let runtime = app.active_runtime().unwrap().expect("a runtime is active");
    assert_eq!(runtime.state, RuntimeState::Active);

    // On Unix the stub exits 127, so it cannot be healthy. On Windows the
    // shell script cannot execute at all, which is also not healthy. Either
    // way a runtime that will not run must say so.
    if cfg!(unix) {
        assert_eq!(
            runtime.healthy,
            Some(false),
            "a runtime that exits 127 must not be reported healthy: {runtime:?}"
        );
        assert!(
            runtime.problem.is_some(),
            "and must carry the reason: {runtime:?}"
        );
    }
}

#[test]
fn the_database_manager_url_is_a_path_not_a_port() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    paths.ensure_layout().unwrap();
    let project = plain_project(&temp);

    let app = App::open_at(&paths).expect("opens");

    // Not installed: no URL, and no claim that one exists.
    let dashboard = app.dashboard(Some(&project)).expect("dashboard");
    assert_eq!(dashboard.database_ui_url, None);

    // Installed: mounted into the site at a path, which is what keeps the
    // user-facing address `http://localhost/phpmyadmin` rather than a second
    // port to remember.
    let entry = lambo_core::dbui::entry_path(&paths);
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, "<?php // manager\n").unwrap();

    let app = App::open_at(&paths).expect("opens");
    let dashboard = app.dashboard(Some(&project)).expect("dashboard");
    let url = dashboard.database_ui_url.expect("the manager is installed");
    assert!(
        url.ends_with("/phpmyadmin"),
        "the manager is mounted at a path, not given a port: {url}"
    );
    assert!(
        !url.contains(":8081"),
        "and must not fall back to the separate port: {url}"
    );
}

/// The About screen is where a user reports what they are running and where it
/// lives, so both must come from the engine rather than from a literal in the
/// GUI that goes stale at the next release.
#[test]
fn about_reports_the_build_version_and_the_resolved_paths() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);

    let app = App::open_at(&paths).expect("opens");
    let about = app.about();

    assert_eq!(about.product, "Lambo PHP");
    assert_eq!(
        about.version,
        env!("CARGO_PKG_VERSION"),
        "the GUI must show the version this build actually is"
    );
    assert_eq!(about.home, paths.root());
    assert_eq!(about.data, paths.data_dir());
    assert_eq!(about.config, paths.config_dir());
    assert_eq!(about.logs, paths.logs_dir());

    // Install location and user data must be distinguishable: an upgrade
    // replaces one and must never touch the other.
    assert_ne!(
        about.home, about.data,
        "user data is not the install directory"
    );

    // Serialisable, like every other model the GUI consumes.
    let json = serde_json::to_string(&about).expect("serialises");
    assert!(json.contains("\"version\""), "{json}");
    assert!(json.contains("\"data\""), "{json}");
}

/// A service that failed to start must not read as one that was never started.
///
/// The state file cannot carry the distinction - a record for a process that
/// died is pruned - so without this the GUI would offer `Start` on a service
/// that has just failed and tell the user nothing about why.
#[test]
fn a_failed_start_is_reported_as_failed_not_as_never_started() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    let project = plain_project(&temp);

    let mut app = App::open_at(&paths).expect("opens");

    // Nothing has been attempted yet, so every service is simply not started.
    let before = app.dashboard(Some(&project)).expect("dashboard");
    assert!(
        before
            .services
            .iter()
            .all(|service| service.state == ServiceState::NotStarted),
        "a fresh home has no failures: {before:?}"
    );
    assert!(before.healthy, "nothing has gone wrong yet");

    // Start the database with no database server installed: this fails, and
    // the error names the service.
    let outcome = app.database_start(&project);
    assert!(!outcome.ok, "expected the start to fail");
    assert!(outcome.error.is_some(), "the failure carries a message");

    let after = app.dashboard(Some(&project)).expect("dashboard");
    let failed: Vec<&str> = after
        .services
        .iter()
        .filter(|service| service.state == ServiceState::Failed)
        .map(|service| service.name.as_str())
        .collect();
    assert!(
        !failed.is_empty(),
        "a failed start must surface as Failed: {after:?}"
    );
    assert!(!after.healthy, "a failed service counts against health");

    // Only a service the evidence actually named is blamed.
    for name in &failed {
        assert!(
            app.failure(name).is_some(),
            "the reason is retrievable for {name}"
        );
    }
}

/// A multi-service operation that fails without naming a culprit must not blame
/// every service it touched. Guessed attribution is worse than none: "database
/// failed" when Apache was the problem sends the user to the wrong logs.
#[test]
fn a_failure_is_attributed_only_when_the_evidence_names_a_service() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    let project = plain_project(&temp);

    let mut app = App::open_at(&paths).expect("opens");

    // `up` spans the web server, PHP and the database. With nothing installed
    // it fails, and the failure is reported - but no service is marked Failed
    // unless the error actually said which one.
    let outcome = app.start_all(&project, false);
    assert!(!outcome.ok, "expected the start to fail");

    let dashboard = app.dashboard(Some(&project)).expect("dashboard");
    let failed: Vec<&str> = dashboard
        .services
        .iter()
        .filter(|service| service.state == ServiceState::Failed)
        .map(|service| service.name.as_str())
        .collect();

    // `failed_service` is a free-form label, not necessarily a service name:
    // the failure below is reported as "PHP stable", a runtime description.
    // Only a name that matches a managed service may blame one.
    let managed = ["apache", "php-server", "database", "dbui"];
    match &outcome.failed_service {
        Some(named) if managed.contains(&named.as_str()) => assert_eq!(
            failed,
            vec![named.as_str()],
            "only the service the error named may be marked Failed"
        ),
        // It named a service that does not exist, or nothing at all. Neither
        // justifies blaming a row: the notice carries the message, and marking
        // some service Failed on a guess would send the user to the wrong
        // place. This is the "database failed when Apache failed" failure mode
        // in reverse - a false attribution is as bad as a wrong one.
        other => assert!(
            failed.is_empty(),
            "no service may be marked Failed on a guess: {failed:?} (failed_service={other:?})"
        ),
    }
}

/// A single-target operation needs no naming from the error: "this operation
/// failed" and "this service failed" are the same statement.
#[test]
fn a_single_target_operation_attributes_to_its_target() {
    let temp = TempDir::new();
    let paths = Paths::from_root(&temp.path);
    let project = plain_project(&temp);

    let mut app = App::open_at(&paths).expect("opens");
    let outcome = app.database_start(&project);
    assert!(!outcome.ok, "no database server is installed");

    let dashboard = app.dashboard(Some(&project)).expect("dashboard");
    let failed: Vec<&str> = dashboard
        .services
        .iter()
        .filter(|service| service.state == ServiceState::Failed)
        .map(|service| service.name.as_str())
        .collect();

    assert_eq!(
        failed,
        vec!["database"],
        "the database operation blames the database, and nothing else"
    );
    assert!(
        app.failure("database").is_some(),
        "the reason is retrievable"
    );
}
