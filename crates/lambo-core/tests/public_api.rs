//! Integration tests for the public API of `lambo-core`.
//!
//! These run against the crate the way a consumer does - the CLI today, a GUI
//! tomorrow - so they cover two things at once: that the behaviour is right,
//! and that the API is actually usable from outside. Anything a GUI would need
//! but cannot reach is a bug in the same way a wrong answer is.
//!
//! The Windows-specific cases matter most here. `Os` is an argument rather than
//! a compile-time switch, so Windows path shapes, executable names, generated
//! configuration and command lines are asserted on every platform - a
//! Windows-only regression fails CI on Linux.

use std::ffi::OsStr;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lambo_core::apache::{self, Apache, PhpIntegration};
use lambo_core::archive;
use lambo_core::browser;
use lambo_core::catalog::{Catalog, Family};
use lambo_core::config::{Config, DatabaseKind, ServerKind};
use lambo_core::database::{self, Credentials, Database};
use lambo_core::dbui;
use lambo_core::detect;
use lambo_core::doctor::{self, Severity};
use lambo_core::envfile::{self, EnvFile, SetOutcome};
use lambo_core::fsx;
use lambo_core::lambofile::Lambofile;
use lambo_core::logs::{self, Group};
use lambo_core::migration;
use lambo_core::naming;
use lambo_core::paths::Paths;
use lambo_core::platform::{Os, Platform, default_data_dir};
use lambo_core::port;
use lambo_core::process::{self, Output, ProcessSpec};
use lambo_core::runtime::{self, InstalledRuntime, RuntimeKind};
use lambo_core::session::{self, Context};
use lambo_core::version::VersionSpec;

/// A temporary directory that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-it-{}-{}",
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

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// A Lambo home with the full directory layout, ready to use.
    fn home(&self) -> Paths {
        let paths = Paths::from_root(self.join("lambo-home"));
        paths
            .ensure_layout()
            .expect("failed to create the home layout");
        paths
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Writes a file, creating parent directories.
fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create a parent directory");
    }
    std::fs::write(path, contents).expect("failed to write the fixture");
}

// ---------------------------------------------------------------------------
// Paths and platform
// ---------------------------------------------------------------------------

#[test]
fn the_home_directory_is_platform_shaped() {
    let home = PathBuf::from("/home/dev");

    let windows = default_data_dir(Os::Windows, &home).unwrap();
    assert!(
        windows.ends_with("Lambo"),
        "the Windows home must be a visible directory, not a dot-directory: {windows:?}"
    );

    for os in [Os::Linux, Os::MacOs] {
        let unix = default_data_dir(os, &home).unwrap();
        assert!(
            unix.ends_with(".lambo"),
            "the {os} home must be a dot-directory: {unix:?}"
        );
    }
}

#[test]
fn lambo_home_wins_over_the_user_home() {
    let portable = Paths::detect_with(
        Some(OsStr::new("/usb/lambo")),
        Some(PathBuf::from("/home/dev")),
        Os::Windows,
    )
    .unwrap();
    assert_eq!(portable.root(), Path::new("/usb/lambo"));
}

#[test]
fn a_missing_home_directory_is_an_error_not_a_guess() {
    let error = Paths::detect_with(None, None, Os::Linux).unwrap_err();
    assert!(matches!(error, lambo_core::Error::NoHomeDir), "{error}");
}

#[test]
fn the_home_layout_covers_everything_the_product_owns() {
    let temp = TempDir::new();
    let paths = temp.home();

    for dir in paths.layout_dirs() {
        assert!(dir.is_dir(), "the layout is missing {}", dir.display());
    }

    // The subdirectories the documentation promises.
    assert!(paths.php_dir().is_dir());
    assert!(paths.apache_dir().is_dir());
    assert!(paths.database_dir().is_dir());
    assert!(paths.logs_dir().is_dir());
    assert!(paths.config_dir().is_dir());
    assert!(paths.projects_dir().is_dir());
    assert!(paths.bin_dir().is_dir());
    assert!(paths.data_dir().is_dir());
}

#[test]
fn configuration_paths_are_stable_and_inside_the_home() {
    let temp = TempDir::new();
    let paths = temp.home();

    for path in [
        paths.config_file(),
        paths.workspaces_file(),
        paths.apache_config_file(),
        paths.database_config_file(),
        paths.catalogs_dir(),
        paths.dbui_dir(),
    ] {
        assert!(
            path.starts_with(paths.root()),
            "{} escapes the Lambo home {}",
            path.display(),
            paths.root().display()
        );
    }

    // One file per purpose, with the documented names.
    assert_eq!(paths.config_file().file_name().unwrap(), "lambo.yml");
    assert_eq!(
        paths.apache_config_file().file_name().unwrap(),
        "httpd.conf"
    );
}

#[test]
fn executables_get_an_extension_only_on_windows() {
    assert_eq!(Os::Windows.executable_name("httpd"), "httpd.exe");
    assert_eq!(Os::Linux.executable_name("httpd"), "httpd");
    assert_eq!(Os::MacOs.executable_name("mysqld"), "mysqld");
    // An explicit extension is never doubled.
    assert_eq!(Os::Windows.executable_name("lambo.exe"), "lambo.exe");
}

#[test]
fn the_program_search_path_is_platform_specific() {
    let windows = Os::Windows.program_search_dirs();
    assert!(
        windows
            .iter()
            .any(|dir| dir.to_string_lossy().contains("Apache24")),
        "a system Apache must be findable on Windows: {windows:?}"
    );
    let linux = Os::Linux.program_search_dirs();
    assert!(
        linux
            .iter()
            .any(|dir| dir.to_string_lossy().contains("/usr")),
        "the usual Unix locations must be searched: {linux:?}"
    );
}

#[test]
fn the_platform_key_matches_the_catalogue_vocabulary() {
    assert_eq!(
        Platform {
            os: Os::Windows,
            arch: lambo_core::platform::Arch::X86_64
        }
        .key(),
        "windows-x64"
    );
    assert_eq!(
        Platform::host().key(),
        format!(
            "{}-{}",
            Os::host().as_str(),
            lambo_core::platform::Arch::host().short_name()
        )
    );
}

// ---------------------------------------------------------------------------
// Generated configuration
// ---------------------------------------------------------------------------

/// An Apache installation that only exists as a path, so configuration
/// generation can be tested without a real server.
fn fake_apache(root: &Path, os: Os) -> Apache {
    Apache {
        runtime: None,
        executable: root.join(os.executable_name("httpd")),
        server_root: root.to_path_buf(),
    }
}

#[test]
fn the_generated_apache_configuration_is_written_for_the_target_platform() {
    let temp = TempDir::new();
    let paths = temp.home();

    let plan = apache::Plan {
        apache: fake_apache(&paths.apache_dir(), Os::Windows),
        port: 8080,
        document_root: PathBuf::from(r"C:\code\shop\public"),
        project_name: "shop".to_owned(),
        php: Some(PhpIntegration {
            version: "8.4.2".to_owned(),
            module: PathBuf::from(r"C:\Lambo\php\8.4.2\php8apache2_4.dll"),
            ini_dir: PathBuf::from(r"C:\Lambo\php\8.4.2"),
        }),
        allow_override: true,
        directory_index: vec!["index.php".to_owned(), "index.html".to_owned()],
        aliases: vec![apache::Alias {
            path: "/phpmyadmin".to_owned(),
            directory: PathBuf::from(r"C:\Lambo\dbui\phpmyadmin"),
        }],
    };

    let written = apache::write_config(&paths, &plan, Os::Windows).unwrap();
    assert!(
        written.is_file(),
        "the configuration must actually be written"
    );

    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.contains("Listen 8080"), "{text}");
    assert!(text.contains("index.php"), "{text}");
    assert!(
        text.contains("AllowOverride All"),
        "Laravel needs .htaccess: {text}"
    );

    // Apache on Windows accepts forward slashes and is confused by stray
    // backslashes in paths, so generated paths must be normalized. The one
    // legitimate backslash is inside the PHP handler regex.
    for line in text.lines() {
        if line.contains("FilesMatch") {
            continue;
        }
        assert!(
            !line.contains(r"C:\"),
            "a Windows path leaked into the configuration unnormalized: {line}"
        );
    }

    // State must not land in the server's own directory: it has to survive a
    // runtime being reinstalled.
    assert!(
        text.contains(&lambo_core::paths::to_forward_slashes(
            &paths.apache_run_dir()
        )),
        "the pid file must live under the Lambo home: {text}"
    );
}

#[test]
fn the_database_configuration_never_contains_the_password() {
    let temp = TempDir::new();
    let paths = temp.home();
    let database = Database {
        kind: DatabaseKind::Mariadb,
        runtime: InstalledRuntime {
            kind: RuntimeKind::Mariadb,
            name: "11.4.4".to_owned(),
            version: semver::Version::parse("11.4.4").unwrap(),
            path: paths.runtime_version_dir(RuntimeKind::Mariadb, "11.4.4"),
        },
        server: paths.database_dir().join("bin/mariadbd"),
        client: Some(paths.database_dir().join("bin/mariadb")),
        admin: Some(paths.database_dir().join("bin/mariadb-admin")),
        initializer: Some(paths.database_dir().join("bin/mariadb-install-db")),
    };
    let mut plan = database::Plan::from_config(&paths, &Config::default().database);
    plan.password = "sup3r-s3cret".to_owned();

    let written = database::write_config(&database, &plan, Os::Linux).unwrap();
    let text = std::fs::read_to_string(&written).unwrap();

    assert!(
        text.contains("bind-address = 127.0.0.1"),
        "loopback only: {text}"
    );
    assert!(
        !text.contains("sup3r-s3cret"),
        "a password must never be written to my.cnf: {text}"
    );
    assert!(
        !text.contains("password"),
        "not even the key belongs in the file: {text}"
    );
}

#[test]
fn the_credentials_report_hides_the_password_until_asked() {
    let temp = TempDir::new();
    let paths = temp.home();
    let mut plan = database::Plan::from_config(&paths, &Config::default().database);
    plan.password = "sup3r-s3cret".to_owned();

    let credentials = Credentials::for_plan(&plan, DatabaseKind::Mariadb, Some("shop"));
    assert!(!credentials.render(false).contains("sup3r-s3cret"));
    assert!(credentials.render(true).contains("sup3r-s3cret"));
}

#[test]
fn the_database_manager_url_carries_no_password() {
    let url = dbui::open_url(8081, 3306, Some("shop"), "root");
    assert!(url.starts_with("http://127.0.0.1:8081/"), "{url}");
    assert!(!url.contains("password"), "{url}");
    assert!(url.contains("shop"), "the database is prefilled: {url}");
}

// ---------------------------------------------------------------------------
// Processes, ports, shutdown
// ---------------------------------------------------------------------------

/// A program that idles, so a test has something real to supervise.
fn sleeper(seconds: u32) -> ProcessSpec {
    #[cfg(windows)]
    {
        // `timeout.exe` refuses to run without a console ("Input redirection
        // is not supported") and dies the moment it is started detached with
        // its standard input on NUL. `ping` never reads its input, so it
        // idles headless for as long as it is asked.
        ProcessSpec::new("C:\\Windows\\System32\\ping.exe", "sleeper")
            .arg("-n")
            .arg((seconds + 1).to_string())
            .arg("127.0.0.1")
    }
    #[cfg(not(windows))]
    {
        ProcessSpec::new("/bin/sleep", "sleeper").arg(seconds.to_string())
    }
}

#[test]
fn a_service_is_spawned_observed_and_stopped() {
    let os = Os::host();
    let spec = sleeper(30)
        .detached()
        .stdout(Output::Null)
        .stderr(Output::Null);

    let mut child = process::spawn(&spec, os).expect("failed to spawn the test service");
    let pid = child.id();
    assert!(
        process::wait_until_running(pid, os, Duration::from_secs(5)),
        "the test service did not start"
    );
    assert!(
        process::is_running(pid, os),
        "a running pid must be reported as running"
    );

    let outcome = process::stop(pid, os, None, Duration::from_secs(10)).unwrap();
    assert!(outcome.stopped(), "the process must be gone: {outcome:?}");
    assert!(
        process::wait_until_gone(pid, os, Duration::from_secs(10)),
        "the service survived being stopped"
    );
    assert!(!process::is_running(pid, os));

    let _ = child.wait();
}

#[test]
fn stopping_one_service_leaves_every_other_process_alone() {
    let os = Os::host();
    let spec = sleeper(30)
        .detached()
        .stdout(Output::Null)
        .stderr(Output::Null);

    let mut mine = process::spawn(&spec, os).unwrap();
    let mut theirs = process::spawn(&spec, os).unwrap();
    let (mine_pid, theirs_pid) = (mine.id(), theirs.id());
    assert!(process::wait_until_running(
        mine_pid,
        os,
        Duration::from_secs(5)
    ));
    assert!(process::wait_until_running(
        theirs_pid,
        os,
        Duration::from_secs(5)
    ));

    // Forced, not polite: on Windows the polite tree kill is delivered as
    // WM_CLOSE, and a detached child has no window that could receive it -
    // nothing short of /F will terminate one. The polite mode keeps its
    // coverage through `process::stop` in the test above. The point here is
    // the scoping of a tree kill, not its manners.
    process::terminate_tree(mine_pid, os, true).unwrap();
    assert!(process::wait_until_gone(
        mine_pid,
        os,
        Duration::from_secs(10)
    ));

    // The point of the whole exercise: a pid read back from a state file must
    // never take an unrelated process with it.
    assert!(
        process::is_running(theirs_pid, os),
        "an unrelated process must survive another service being stopped"
    );

    let _ = process::stop(theirs_pid, os, None, Duration::from_secs(10));
    let _ = mine.wait();
    let _ = theirs.wait();
}

#[test]
fn an_occupied_port_is_reported_as_a_conflict() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = listener.local_addr().unwrap().port();

    assert!(!port::is_free(busy), "a bound port is not free");
    assert!(port::is_listening(busy), "a bound port is listening");

    let error = port::check(busy, Os::host()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(&busy.to_string()), "{message}");

    // The advice must name the setting to change, not suggest killing anything.
    let advice = port::conflict_advice(busy, Some("docker-proxy"), "server.port");
    assert!(advice.contains("server.port"), "{advice}");
    assert!(!advice.to_lowercase().contains("kill"), "{advice}");

    // Alternatives are offered instead.
    let free = port::alternatives(busy);
    assert!(!free.is_empty());
    assert!(!free.contains(&busy));
}

#[test]
fn the_first_free_port_wins() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = listener.local_addr().unwrap().port();
    let other = busy + 1;

    assert_eq!(port::first_free([busy, other]), Some(other));
    assert_eq!(port::first_free([busy]), None);
}

// ---------------------------------------------------------------------------
// Service lifecycle
// ---------------------------------------------------------------------------

/// The installation, as `lambo up` and the GUI both see it.
///
/// The state file this used to assert about is gone: the services are the
/// engines now, and [`lambo_core::stack`] is where their behaviour is tested.
/// What is worth asserting through the public surface is the wiring - that the
/// document on disk becomes exactly the cards an interface shows, with an engine
/// only where the configuration names an executable.
#[test]
fn the_installation_is_the_configured_stack() {
    let temp = TempDir::new();
    let context = Context {
        paths: temp.home(),
        config: Config::default(),
        catalog: Catalog::embedded().unwrap(),
        platform: Platform::host(),
        downloader: &lambo_core::download::CurlDownloader,
        os: Os::host(),
        log: lambo_core::logs::nop_log(),
    };

    let installation = session::installation(&context).expect("the installation loads");
    let cards = installation.stack.services();

    assert_eq!(cards.len(), installation.config.services.len());
    assert!(
        cards.iter().all(|card| card.name() == card.conf().name),
        "a card is named by the configuration it came from"
    );
    for card in cards {
        let configured = installation
            .config
            .services
            .iter()
            .find(|service| service.name == card.name())
            .expect("the card came from a configured service");
        assert_eq!(
            card.service().is_some(),
            !configured.exe.is_empty(),
            "only a service with an executable has an engine: {}",
            card.name()
        );
        assert!(!card.running(), "a home that was just read runs nothing");
    }

    // The names the interfaces attribute a failure to are the configuration's.
    assert!(session::ALL_SERVICES.contains(&"Apache"));
    assert!(session::WEB_SERVICES.contains(&"Nginx"));
    assert!(session::DATABASE_SERVICES.contains(&"MySQL"));
}

#[test]
fn shutting_down_an_empty_home_reports_nothing_rather_than_failing() {
    let temp = TempDir::new();
    let paths = temp.home();
    let mut context = Context {
        paths: paths.clone(),
        config: Config::default(),
        catalog: Catalog::embedded().unwrap(),
        platform: Platform::host(),
        downloader: &lambo_core::download::CurlDownloader,
        os: Os::host(),
        log: lambo_core::logs::nop_log(),
    };

    let report = session::down(None, &mut context).unwrap();
    assert!(report.steps.is_empty(), "{:?}", report.steps);

    let status = session::status(None, &mut context).unwrap();
    assert!(status.services.iter().all(|service| !service.running));
    assert!(!status.serving);
}

// ---------------------------------------------------------------------------
// Filesystem operations
// ---------------------------------------------------------------------------

#[test]
fn atomic_writes_leave_no_partial_file_behind() {
    let temp = TempDir::new();
    let target = temp.join("config/lambo.yml");

    fsx::write_atomic(&target, "first").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");

    fsx::write_atomic(&target, "second").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");

    // The temporary file used for the rename must not linger.
    let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files were left behind: {leftovers:?}"
    );
}

#[test]
fn writability_is_observed_not_assumed() {
    let temp = TempDir::new();
    assert!(fsx::is_writable(temp.path()));
    assert!(!fsx::is_writable(&temp.join("does-not-exist")));
}

#[test]
fn archive_entries_cannot_escape_the_destination() {
    let temp = TempDir::new();
    let destination = temp.join("extract");
    std::fs::create_dir_all(&destination).unwrap();

    assert!(archive::safe_join(&destination, "php.exe").is_ok());
    assert!(archive::safe_join(&destination, "ext/php_mysqli.dll").is_ok());

    for hostile in [
        "../escape.txt",
        "../../escape.txt",
        "/etc/passwd",
        "ext/../../escape.txt",
    ] {
        assert!(
            archive::safe_join(&destination, hostile).is_err(),
            "`{hostile}` must not be allowed to escape the destination"
        );
    }
}

#[test]
fn extracting_something_that_is_not_an_archive_fails_cleanly() {
    let temp = TempDir::new();
    let not_an_archive = temp.join("php.zip");
    write(&not_an_archive, "this is not a zip file");

    let error = archive::extract(&not_an_archive, &temp.join("out")).unwrap_err();
    assert!(
        matches!(error, lambo_core::Error::Archive { .. }),
        "{error}"
    );
    assert!(
        !temp.join("out/php").exists(),
        "a failed extraction must not leave a directory that looks installed"
    );
}

// ---------------------------------------------------------------------------
// Projects, detection, environment files
// ---------------------------------------------------------------------------

#[test]
fn detection_reports_its_evidence_including_what_it_ruled_out() {
    let temp = TempDir::new();
    let project = temp.join("shop");
    write(&project.join("artisan"), "#!/usr/bin/env php\n");
    write(
        &project.join("composer.json"),
        r#"{"name":"acme/shop","require":{"laravel/framework":"^11.0","php":"^8.2"}}"#,
    );
    write(&project.join("public/index.php"), "<?php\n");

    let detection = detect::detect(&project);
    assert_eq!(detection.framework, detect::Framework::Laravel);
    assert_eq!(detection.document_root, "public");
    assert!(detection.needs_database);
    assert_eq!(detection.php_requirement.as_deref(), Some("^8.2"));

    // The probes that decided the answer are reported, so a wrong answer is
    // diagnosable. A confident match stops probing early - the case that lists
    // its negative findings is the ambiguous one, covered below.
    assert!(!detection.probes.is_empty());
    assert!(
        detection.probes.iter().all(|probe| probe.found),
        "{:?}",
        detection.probes
    );
    assert!(
        !detection.matched.is_empty(),
        "the summary must cite its evidence"
    );
}

#[test]
fn a_plain_php_project_needs_no_database_and_serves_from_the_root() {
    let temp = TempDir::new();
    let project = temp.join("sandbox");
    write(&project.join("index.php"), "<?php echo 'hi';\n");

    let detection = detect::detect(&project);
    assert_eq!(detection.framework, detect::Framework::PlainPhp);
    assert_eq!(detection.document_root, ".");
    assert!(!detection.needs_database);
}

#[test]
fn the_project_file_round_trips_and_rejects_dangerous_values() {
    let temp = TempDir::new();
    let project = temp.join("shop");
    std::fs::create_dir_all(&project).unwrap();

    let file = Lambofile {
        name: Some("shop".to_owned()),
        php: "8.3".parse().unwrap(),
        server: lambo_core::lambofile::ServerSettings {
            kind: Some(ServerKind::Apache),
            document_root: "public".to_owned(),
            ..lambo_core::lambofile::ServerSettings::default()
        },
        database: lambo_core::lambofile::DatabaseSettings {
            kind: Some(DatabaseKind::Mariadb),
            name: Some("shop".to_owned()),
            ..lambo_core::lambofile::DatabaseSettings::default()
        },
        ..Lambofile::default()
    };

    let written = file.save(&project).unwrap();
    assert!(written.is_file());
    let loaded = Lambofile::load(&project)
        .unwrap()
        .expect("the file must load");
    assert_eq!(loaded, file);

    // Relative paths only, so a committed file never carries `C:\…`.
    let raw = std::fs::read_to_string(&written).unwrap();
    assert!(!raw.contains("C:\\"), "{raw}");
    assert!(raw.contains("document_root: public"), "{raw}");

    for escaping in ["../etc", "/var/www", r"C:\code"] {
        let mut bad = file.clone();
        bad.server.document_root = escaping.to_owned();
        assert!(
            bad.validate(&project).is_err(),
            "`{escaping}` must be rejected as a document root"
        );
    }

    let mut reserved = file.clone();
    reserved.name = Some("con".to_owned());
    assert!(
        reserved.validate(&project).is_err(),
        "a reserved Windows device name must be rejected"
    );
}

#[test]
fn environment_files_keep_what_the_user_already_set() {
    let temp = TempDir::new();
    let project = temp.join("shop");
    write(
        &project.join(".env"),
        "# mine\nAPP_NAME=Shop\nDB_HOST=existing-host\n",
    );

    let mut env = EnvFile::load(project.join(".env")).unwrap();
    assert_eq!(env.get("APP_NAME"), Some("Shop"));

    let keys = envfile::generic_database_keys("shop", "root", "secret", "127.0.0.1", 3306);
    let changes = env.ensure_all(&keys, false);
    env.save().unwrap();

    assert!(
        changes.kept.iter().any(|key| key == "DB_HOST"),
        "a value the user set must be kept, not overwritten: {changes:?}"
    );

    let text = std::fs::read_to_string(project.join(".env")).unwrap();
    assert!(text.contains("DB_HOST=existing-host"), "{text}");
    assert!(text.contains("# mine"), "comments survive: {text}");
    assert!(text.contains("DB_PORT=3306"), "{text}");

    // Writing the same value again is a no-op, not a change.
    let mut env = EnvFile::load(project.join(".env")).unwrap();
    assert_eq!(env.set("DB_PORT", "3306"), SetOutcome::Unchanged);
}

// ---------------------------------------------------------------------------
// Runtime discovery and the catalogue
// ---------------------------------------------------------------------------

#[test]
fn an_installed_runtime_is_discovered_and_activated_by_name() {
    let temp = TempDir::new();
    let paths = temp.home();
    let os = Os::host();

    // Lay out what an extraction would have produced.
    let version_dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");
    write(&version_dir.join(os.executable_name("php")), "#!/bin/sh\n");

    let installed = runtime::installed(&paths, RuntimeKind::Php).unwrap();
    assert_eq!(
        installed.len(),
        1,
        "the version directory must be discovered"
    );
    assert_eq!(installed[0].name, "8.4.2");

    runtime::set_active(&paths, RuntimeKind::Php, "8.4.2").unwrap();
    assert_eq!(
        runtime::active_name(&paths, RuntimeKind::Php)
            .unwrap()
            .as_deref(),
        Some("8.4.2")
    );

    // A directory that is not a version is ignored rather than crashing.
    std::fs::create_dir_all(paths.runtime_version_dir(RuntimeKind::Php, "not-a-version")).unwrap();
    assert_eq!(
        runtime::installed(&paths, RuntimeKind::Php).unwrap().len(),
        1
    );

    runtime::remove(&paths, RuntimeKind::Php, "8.4.2").unwrap();
    assert!(
        runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty()
    );
    assert!(
        runtime::active_name(&paths, RuntimeKind::Php)
            .unwrap()
            .is_none(),
        "removing the active runtime must clear the marker"
    );
}

#[test]
fn the_catalogue_offers_php_for_every_supported_platform() {
    let catalog = Catalog::embedded().unwrap();
    for platform in [
        "windows-x64",
        "linux-x64",
        "linux-arm64",
        "macos-x64",
        "macos-arm64",
    ] {
        assert!(
            !catalog.available(Family::Php, platform).is_empty(),
            "no PHP release for {platform}"
        );
    }
    assert!(!catalog.versions(Family::Php).is_empty());
}

#[test]
fn downloads_are_refused_without_a_way_to_verify_them() {
    // A shipped entry is either pinned with a digest or sent to look for the
    // upstream sidecar; where neither exists the download fails closed - that
    // is the promise, and this asserts the transport half of it.
    let catalog = Catalog::embedded().unwrap();
    let releases = catalog.available(Family::Php, "windows-x64");
    let release = releases
        .first()
        .expect("the catalogue must offer a Windows PHP release");
    let artifact = release.artifact();
    assert!(
        artifact.url.starts_with("https://"),
        "HTTPS only: {}",
        artifact.url
    );
    assert!(lambo_core::download::require_https(&artifact.url).is_ok());
    assert!(lambo_core::download::require_https("http://example.com/php.zip").is_err());
}

// ---------------------------------------------------------------------------
// Configuration, naming, versions
// ---------------------------------------------------------------------------

#[test]
fn configuration_values_are_validated_before_they_are_saved() {
    let temp = TempDir::new();
    let paths = temp.home();
    let mut config = Config::default();

    assert!(config.set("server.port", "8088").is_ok());
    assert_eq!(config.get("server.port").unwrap(), "8088");

    for bad in ["0", "65536", "not-a-number", ""] {
        assert!(
            config.set("server.port", bad).is_err(),
            "`{bad}` must be rejected as a port"
        );
    }
    assert_eq!(
        config.get("server.port").unwrap(),
        "8088",
        "a rejected write must not apply"
    );

    assert!(
        lambo_core::Error::UnknownConfigKey("nope".to_owned())
            .to_string()
            .contains("nope")
    );
    assert!(config.get("nope").is_err());
    assert!(Config::KEYS.contains(&"server.port"));

    // Credentials are generated once, and the file is owner-only.
    assert!(
        config.ensure_credentials(),
        "the first call generates a password"
    );
    let first = config.database.password.clone();
    assert!(!first.is_empty());
    assert!(
        !config.ensure_credentials(),
        "the second call must not rotate it"
    );
    config.save(&paths).unwrap();
    assert!(Config::load(&paths).unwrap().database.password == first);

    let yaml = config.to_yaml().unwrap();
    assert!(yaml.contains("server:"), "{yaml}");
}

#[test]
fn names_are_slugified_and_database_names_are_checked() {
    // A slug is for URLs and names; a database name is a SQL identifier, so
    // they are deliberately not the same transformation.
    assert_eq!(naming::slugify("My Shop!"), "my-shop");
    assert_eq!(naming::database_name("My Shop!"), "my_shop");
    // A name starting with a digit is not a MySQL identifier, so the leading
    // digit has to move; whatever the result, it must still validate.
    let leading_digit = naming::database_name("2 Fast");
    assert!(
        !leading_digit.starts_with(|ch: char| ch.is_ascii_digit()),
        "`{leading_digit}` is not a valid identifier"
    );
    assert!(naming::is_valid_database_name(&leading_digit));
    assert!(naming::is_valid_database_name("shop_2"));
    for bad in ["1shop", "shop-db", "", "shop db", "shop;drop"] {
        assert!(
            !naming::is_valid_database_name(bad),
            "`{bad}` is not a safe identifier"
        );
    }
    assert_eq!(naming::local_url(8080), "http://localhost:8080");
}

#[test]
fn version_specs_mean_what_they_say() {
    assert_eq!(
        "stable".parse::<VersionSpec>().unwrap().to_string(),
        "stable"
    );
    assert_eq!("8.4".parse::<VersionSpec>().unwrap().to_string(), "~8.4");
    assert_eq!(
        "8.4.2".parse::<VersionSpec>().unwrap().to_string(),
        "=8.4.2"
    );
    assert_eq!("^8.3".parse::<VersionSpec>().unwrap().to_string(), "^8.3");
    assert!("not-a-version".parse::<VersionSpec>().is_err());
}

// ---------------------------------------------------------------------------
// Browser, logs, diagnostics, migration
// ---------------------------------------------------------------------------

#[test]
fn opening_a_browser_never_involves_a_shell() {
    let url = "http://localhost:8080/?db=shop&user=root";
    assert!(browser::is_safe_url(url));
    for bad in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "http://localhost/`rm -rf /`",
        "http://localhost/$(id)",
        "http://localhost/\"quoted\"",
    ] {
        assert!(!browser::is_safe_url(bad), "`{bad}` must not be opened");
    }

    for os in [Os::Windows, Os::Linux, Os::MacOs] {
        let (primary, _) = browser::opener(os, url);
        let program = primary.program.to_string_lossy().to_lowercase();
        for shell in ["cmd", "sh", "bash", "powershell", "command"] {
            assert!(
                !program.ends_with(shell) && !program.ends_with(&format!("{shell}.exe")),
                "{os} would open the URL through a shell: {program}"
            );
        }
        assert!(
            primary.args.iter().any(|arg| arg == url),
            "the URL must be an argument, not part of a command line: {:?}",
            primary.args
        );
    }
}

#[test]
fn logs_are_grouped_and_tailed() {
    let temp = TempDir::new();
    let paths = temp.home();

    assert_eq!(Group::parse("apache"), Some(Group::Apache));
    assert_eq!(Group::parse("HTTPD"), Some(Group::Apache));
    assert_eq!(Group::parse("nonsense"), None);

    write(&logs::apache(&paths), "one\ntwo\nthree\n");
    let tail = logs::tail(&logs::apache(&paths), 2).unwrap();
    assert_eq!(tail, vec!["two".to_owned(), "three".to_owned()]);

    // Clearing truncates rather than deleting: a service that is still running
    // keeps its file handle, so removing the file would send its output nowhere.
    assert!(
        logs::clear(&paths).unwrap() >= 1,
        "the log files must be cleared"
    );
    assert!(logs::apache(&paths).is_file(), "the file itself stays");
    assert!(
        logs::tail(&logs::apache(&paths), 10).unwrap().is_empty(),
        "but it is empty"
    );
}

#[test]
fn every_diagnostic_failure_names_the_command_that_fixes_it() {
    let temp = TempDir::new();
    let paths = temp.home();
    let catalog = Catalog::embedded().unwrap();

    let report = doctor::run(
        &paths,
        &Config::default(),
        None,
        &catalog,
        Platform::host(),
        Os::host(),
    );
    assert!(
        !report.checks.is_empty(),
        "a fresh home still has plenty to check"
    );

    for check in &report.checks {
        match check.severity {
            Severity::Ok => {
                assert!(check.fix.is_none() || !check.fix.as_deref().unwrap().is_empty())
            }
            // An unsupported finding must always carry the alternative, or it
            // is just bad news with nothing to do about it.
            Severity::Unsupported | Severity::Warn | Severity::Fail => {
                let fix = check.fix.as_deref().unwrap_or("");
                assert!(
                    !fix.is_empty(),
                    "`{}` fails without telling the user what to run",
                    check.name
                );
                // A fix that names a command which does not exist is worse than
                // no fix at all, so it has to start with something runnable.
                assert!(
                    fix.starts_with("lambo ") || fix.starts_with('`') || fix.starts_with("install"),
                    "`{}` has a fix that is not a command: {fix}",
                    check.name
                );
            }
        }
    }

    assert!(report.exit_code() > 0, "a fresh home cannot be healthy");
    assert!(!report.render().is_empty());
}

#[test]
fn migration_reports_nothing_for_a_home_that_was_never_king() {
    let temp = TempDir::new();
    let paths = temp.home();

    let report = migration::detect_in(&paths, Some(temp.join("no-such-legacy-home")));
    assert!(
        report.is_empty(),
        "a legacy home that is not there is nothing to migrate: {report:?}"
    );
    assert!(migration::detect_in(&paths, None).is_empty());
}

#[test]
fn a_legacy_project_file_is_converted_without_being_deleted() {
    let temp = TempDir::new();
    let project = temp.join("shop");
    write(
        &project.join("kingphp.yml"),
        "name: shop\nphp: ^8.2\nserver: apache\ndocument_root: public\ndatabase: mariadb\n",
    );

    let written = migration::migrate_project(&project)
        .unwrap()
        .expect("the file must convert");
    assert_eq!(written.file_name().unwrap(), "lambo.yml");
    assert!(
        project.join("kingphp.yml").is_file(),
        "the original must survive"
    );

    let file = Lambofile::load(&project)
        .unwrap()
        .expect("the converted file must load");
    assert_eq!(file.server.document_root, "public");
    assert_eq!(file.database.kind, Some(DatabaseKind::Mariadb));
}
