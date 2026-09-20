//! The localhost service lifecycle, verified end to end.
//!
//! This is the acceptance criterion for Lambo's core promise:
//!
//! ```text
//! lambo up  →  a TCP listener exists  →  an HTTP request succeeds
//!           →  lambo status says running  →  lambo down
//!           →  the listener is gone  →  the process is gone
//! ```
//!
//! Every step is observed, not inferred. A test that asserted only "a process
//! was spawned" would pass just as happily against a server that never bound a
//! port, and that is precisely the failure Lambo must never report as success.
//!
//! # What drives what
//!
//! The lifecycle is the one the whole product runs on:
//!
//! ```text
//! session::up / session::down / session::status
//!        → stack::Stack (the services of the installation)
//!        → service::Service (one process and the rules around it)
//!        → process / platform
//! ```
//!
//! The fixture is installed twice, in the two places a real installation puts
//! programs: as `php/8.4.2/php`, the project's runtime, and as `bin/fixture/`, a
//! service entry of the installation's own configuration. A `server.kind: php`
//! project is served by the first, which is what `session::up` starts and what
//! the tests below observe. Nothing here reimplements the lifecycle, and nothing
//! here knows a state file: what is running is what the engine says is running.
//!
//! # What is a fixture and what is real
//!
//! Real PHP, Apache and MariaDB artifacts cannot be downloaded in this
//! environment, so the server process is `lambo-fixture-server`: a small
//! deterministic HTTP server. The **production** engine spawns it, supervises
//! it, reports it and stops it. What this does *not* prove is that real Apache
//! or PHP behaves this way - it proves Lambo's supervision of a managed child
//! process is correct.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lambo_core::catalog::Catalog;
use lambo_core::config::{Config, DatabaseKind, ServerKind};
use lambo_core::download::LocalDownloader;
use lambo_core::http;
use lambo_core::logs::LogFn;
use lambo_core::panel::{PanelConfig, ServiceConf};
use lambo_core::paths::Paths;
use lambo_core::platform::{Arch, Os, Platform};
use lambo_core::port;
use lambo_core::process;
use lambo_core::project::Project;
use lambo_core::runtime::{self, RuntimeKind};
use lambo_core::service::HostService;
use lambo_core::session::{self, Context};
use lambo_core::stack::Stack;
use lambo_core::vhost::VhostForm;

/// The body the fixture server returns.
const FIXTURE_BODY: &str = "Lambo PHP fixture OK";

/// How long a test waits for a server to start or a port to be released.
///
/// Generous enough for a loaded CI machine, short enough that a hang is
/// reported rather than endured.
const WAIT: Duration = Duration::from_secs(20);

/// How often to poll while waiting.
const POLL: Duration = Duration::from_millis(50);

/// The name of the service the fixture is installed as in the installation's
/// own configuration.
///
/// A `server.kind: php` project is served by its own `php -S` process rather
/// than by a service of the installation, so this entry is not what the tests
/// below observe; it keeps the document shaped like a real home's and gives the
/// start-up sweep a program under `bin/` to decide about.
const SERVICE: &str = "Fixture";

/// The name the session reports the project's own server under.
const SERVER: &str = session::PROJECT_SERVER;

/// The version the fixture runtime is installed as.
///
/// The fixture answers `php -v` with its own `DEFAULT_VERSION`, so the two have
/// to agree for the runtime health check to accept it.
const RUNTIME_VERSION: &str = "8.4.2";

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// A temporary directory that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-life-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("failed to create the temporary directory");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A process a test started by hand, killed however the test ends.
///
/// Two tests need a process Lambo did not start - one that must survive `down`,
/// one that the sweep must clean up - and a test that fails before its own
/// cleanup would otherwise leave that process running, holding a port that a
/// later run then trips over.
struct ChildGuard(std::process::Child);

impl ChildGuard {
    fn spawn(exe: &Path, port: u16, docroot: &Path) -> Self {
        let child = std::process::Command::new(exe)
            .arg("-S")
            .arg(format!("127.0.0.1:{port}"))
            .arg("-t")
            .arg(docroot)
            .spawn()
            .expect("the process starts");
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A sink for the lines the engine logs.
///
/// The engine narrates what it does as it does it, which is the only record of
/// *why* something failed - so the tests keep the lines and print them when an
/// assertion fails.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn function(&self) -> LogFn {
        let sink = Arc::clone(&self.0);
        Arc::new(move |line: &str| {
            sink.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(line.to_owned());
        })
    }

    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// An isolated Lambo home, a project inside it, and the fixture installed as the
/// installation's web server.
struct Harness {
    temp: TempDir,
    paths: Paths,
    project: Project,
    config: Config,
    os: Os,
    log: Log,
}

impl Harness {
    fn new(port: u16) -> Self {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        paths.ensure_layout().expect("layout");
        let os = Os::host();

        // The fixture server, installed where a service of this installation
        // lives: under `bin/`, which is also the directory the start-up sweep
        // watches.
        let bin = temp.join("bin/fixture");
        fs::create_dir_all(&bin).expect("the fixture directory");
        let exe = bin.join(os.executable_name("lambo-fixture-server"));
        fs::copy(fixture_binary(), &exe).expect("the fixture server is installed");
        make_executable(&exe);

        // A plain-PHP project: the document root is the project itself and no
        // database is involved, so the installation's own configuration is the
        // only thing that has to exist.
        let root = temp.join("shop");
        fs::create_dir_all(&root).expect("the project directory");
        fs::write(root.join("index.php"), "<?php echo 'hi';\n").expect("the entry point");
        fs::write(
            root.join("lambo.yml"),
            "server:\n  kind: php\n  document_root: .\ndatabase:\n  kind: none\n",
        )
        .expect("the project file");

        let mut config = Config::default();
        config.server.kind = ServerKind::Php;
        config.server.port = port;
        config.database.kind = DatabaseKind::None;

        // The same binary is the project's PHP runtime. `php::serve_spec`
        // builds exactly the command line the fixture implements - `php -S
        // 127.0.0.1:<port> -t <docroot>` - and the fixture answers the three
        // probes Lambo makes of a runtime (`-v`, `--ini`, `-m`), so
        // `session::up` resolves it and health-checks it the way it does a real
        // download. It lives under `php/`: the start-up sweep owns `<home>/bin/`
        // and must leave a running project server alone.
        let php_dir = paths.runtime_dir(RuntimeKind::Php).join(RUNTIME_VERSION);
        fs::create_dir_all(&php_dir).expect("the PHP runtime directory");
        let php = php_dir.join(os.executable_name("php"));
        fs::copy(fixture_binary(), &php).expect("the PHP runtime is installed");
        make_executable(&php);
        runtime::set_active(&paths, RuntimeKind::Php, RUNTIME_VERSION)
            .expect("the PHP runtime is the active one");

        let panel = panel_config(port, &exe, &root, SERVICE);
        panel
            .save(temp.path())
            .expect("the installation's configuration is written");
        // `lambo up` boots the installation's *essential* stack, and the panel's
        // own migration appends every shipped service the file does not mention.
        // A test must not reach the network, so every other service is written
        // as installed - its catalogue check file exists - and disabled. That is
        // also the shape of a real home whose user runs one server by hand.
        satisfy_the_essentials(temp.path());

        Self {
            temp,
            paths,
            project: Project::load(&root).expect("the project loads"),
            config,
            os,
            log: Log::default(),
        }
    }

    fn context(&self) -> Context<'static> {
        Context {
            paths: self.paths.clone(),
            config: self.config.clone(),
            catalog: Catalog::embedded().expect("catalogue"),
            platform: Platform::new(self.os, Arch::X86_64),
            // file:// only, so nothing in a test can reach the network.
            downloader: &LocalDownloader,
            os: self.os,
            log: self.log.function(),
        }
    }

    /// The port the service is configured to bind.
    fn port(&self) -> u16 {
        self.project.http_port(&self.config)
    }

    /// The URL a user would be given for the project.
    fn url(&self) -> String {
        lambo_core::naming::local_url(self.port())
    }

    /// `lambo up`, through the session the CLI uses, without hiding a failure.
    ///
    /// A failure is a legitimate outcome - a port that is taken, a server that
    /// dies on start-up - and a test about failure has to see it.
    fn try_up(&self) -> Result<session::Report, lambo_core::error::Error> {
        let mut context = self.context();
        session::up(&self.project, &mut context, false)
    }

    /// `lambo up`, which must succeed.
    fn up(&self) -> session::Report {
        self.try_up().expect("`lambo up` must not fail")
    }

    /// `lambo down`, through the session the CLI uses.
    fn down(&self) -> session::Report {
        let mut context = self.context();
        session::down(Some(&self.project), &mut context).expect("`lambo down` must not fail")
    }

    /// `lambo down`, without a panic when it fails: the drop guard's own path.
    fn stop_project_server(&self) -> Result<(), lambo_core::error::Error> {
        let mut context = self.context();
        session::down(Some(&self.project), &mut context).map(|_| ())
    }

    /// `lambo status` for the project.
    fn status(&self) -> session::Status {
        let mut context = self.context();
        session::status(Some(&self.project), &mut context).expect("`lambo status` must not fail")
    }

    /// The card the project's own server is reported under.
    fn card(&self) -> session::ServiceStatus {
        self.card_of(SERVER)
    }

    /// The card of one named service, as status reports it.
    fn card_of(&self, name: &str) -> session::ServiceStatus {
        self.status()
            .services
            .into_iter()
            .find(|service| service.name == name)
            .unwrap_or_else(|| panic!("status has no card for `{name}`"))
    }

    /// The pid of the running service, when it is running.
    fn pid(&self) -> Option<u32> {
        self.card().pid.filter(|_| self.card().running)
    }

    fn docroot(&self) -> PathBuf {
        self.temp.join("shop")
    }

    /// Everything worth knowing when an assertion fails.
    ///
    /// A lifecycle test that fails with "assertion failed" is useless; the
    /// interesting facts are whether the process lived, what the engine said
    /// about it, and why.
    fn diagnostics(&self, context: &str) -> String {
        let mut out = format!("--- diagnostics ({context}) ---\n");
        out.push_str(&format!("home: {}\n", self.paths.root().display()));
        out.push_str(&format!("url: {}\n", self.url()));
        out.push_str(&format!(
            "port listening: {}\n",
            port::is_listening(self.port())
        ));
        for service in self.status().services {
            out.push_str(&format!(
                "card: {} {} running={} pid={:?} port={:?} occupant={:?}\n",
                service.name,
                service.state_word(),
                service.running,
                service.pid,
                service.port,
                service.occupant
            ));
        }
        let lines = self.log.lines();
        if lines.is_empty() {
            out.push_str("engine log: (nothing)\n");
        } else {
            out.push_str("engine log:\n");
            for line in lines {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out
    }
}

impl Drop for Harness {
    /// Stops the project's own server, whatever the test did with it.
    ///
    /// A test that fails - or panics half way through - used to leave a real
    /// server behind holding its port, and the next run then failed for a reason
    /// that had nothing to do with the code. Cleanup is the product's own
    /// `down`, so it cannot drift from the lifecycle these tests are about.
    fn drop(&mut self) {
        let _ = self.stop_project_server();
    }
}

/// The installation's own configuration: one service, run by the fixture.
///
/// This is a `config.json` like any other, which is the point - the engine reads
/// it the same way it reads a real one, and the name is the fixture's so that a
/// failure cannot be mistaken for Apache's.
fn panel_config(port: u16, exe: &Path, docroot: &Path, name: &str) -> PanelConfig {
    let mut panel = PanelConfig::default_config();
    panel.services = vec![ServiceConf {
        name: name.to_owned(),
        kind: "web".to_owned(),
        exe: exe.display().to_string(),
        // The same command line `php -S` takes, which is what the fixture
        // implements.
        args: vec![
            "-S".to_owned(),
            format!("127.0.0.1:{port}"),
            "-t".to_owned(),
            docroot.display().to_string(),
        ],
        port,
        workdir: docroot.display().to_string(),
        config_file: String::new(),
        enabled: true,
        open_url: String::new(),
        active_version: String::new(),
        env: Vec::new(),
    }];
    panel.settings.active_web_server = name.to_owned();
    panel.settings.auto_start = Vec::new();
    panel
}

/// Writes the check files that make every other shipped component count as
/// installed, so the essential pass installs nothing.
///
/// The catalogue's rule is one file: a component is installed when its check
/// file is there. Writing them is therefore the cheapest honest way to say "this
/// test does not download anything" - and it is the same check the panel's own
/// cards use.
fn satisfy_the_essentials(base_dir: &Path) {
    let mut config = PanelConfig::load(base_dir).expect("the configuration loads");
    for service in &mut config.services {
        if service.name == SERVICE {
            continue;
        }
        service.enabled = false;
        if let Some(component) = lambo_core::catalog_panel::find(&service.name) {
            let check = component.canonical_dir(base_dir).join(component.check_file);
            if let Some(parent) = check.parent() {
                fs::create_dir_all(parent).expect("the component's directory");
            }
            fs::write(&check, "fixture").expect("the component's check file");
        }
    }
    config.save(base_dir).expect("the configuration is written");
}

/// The compiled fixture server, located by Cargo.
fn fixture_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lambo-fixture-server"))
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// A port for one test.
///
/// Handed out from a counter rather than discovered by binding and releasing:
/// `first_free` answers "is it free *now*", and two tests asking in parallel can
/// both be told yes about the same port, after which one server fails to bind
/// and the failure has nothing to do with the code under test. Each test also
/// checks the port is genuinely unused before it starts.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(45_200);

    for _ in 0..400 {
        let candidate = NEXT.fetch_add(1, Ordering::SeqCst);
        if candidate > 45_900 {
            NEXT.store(45_200, Ordering::SeqCst);
            continue;
        }
        if port::is_free(candidate) {
            return candidate;
        }
    }
    panic!("no free port could be allocated for this test");
}

// ---------------------------------------------------------------------------
// Bounded polling helpers
// ---------------------------------------------------------------------------

/// Polls until `predicate` holds, or fails with `context` and diagnostics.
///
/// No fixed sleeps anywhere in this file: a healthy server is observed within
/// one poll interval, and a broken one is reported after a bounded wait.
fn wait_until(harness: &Harness, context: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(POLL);
    }
    panic!(
        "timed out after {WAIT:?} waiting for {context}\n{}",
        harness.diagnostics(context)
    );
}

/// Performs a real HTTP GET and returns status and body.
fn http_get(url: &str) -> Result<(u16, String), String> {
    http::get(url, Duration::from_secs(5))
        .map(|response| (response.status, response.body))
        .map_err(|error| error.to_string())
}

/// Waits for the project's URL to answer, and returns the body it served.
fn expect_serving(harness: &Harness, context: &str) -> String {
    let url = harness.url();
    let mut body = String::new();
    wait_until(harness, context, || match http_get(&url) {
        Ok((200, text)) => {
            body = text;
            true
        }
        _ => false,
    });
    body
}

/// Waits for the port to be released.
fn expect_released(harness: &Harness, context: &str) {
    let port = harness.port();
    wait_until(harness, context, || !port::is_listening(port));
}

// ---------------------------------------------------------------------------
// The acceptance test
// ---------------------------------------------------------------------------

#[test]
fn up_serves_http_and_down_leaves_nothing_behind() {
    let harness = Harness::new(free_port());
    let port = harness.port();

    // Nothing is listening before `up`.
    assert!(
        !port::is_listening(port),
        "port {port} is occupied before the test even starts"
    );
    assert!(!harness.card().running, "nothing is running yet");

    // 1. `lambo up` boots the project's own server.
    let report = harness.up();
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name == SERVER && step.outcome == session::StepOutcome::Done),
        "the report must say the server started: {:?}",
        report.steps
    );
    assert_eq!(
        report.url.as_deref(),
        Some(harness.url().as_str()),
        "the report ends on the project's own URL"
    );

    // 2. A TCP listener exists, and it is the process the engine supervises.
    wait_until(&harness, "the service to bind its port", || {
        port::is_listening(port)
    });
    let pid = harness.pid().expect("the card must report a pid");
    assert!(pid > 0);

    // 3. An HTTP request succeeds, and answers with the fixture's own body.
    let body = expect_serving(&harness, "an HTTP 200 from the service");
    assert_eq!(body, FIXTURE_BODY);

    // 4. `lambo status` agrees: running, with the pid and the port.
    let card = harness.card();
    assert!(card.running, "status must say running: {card:?}");
    assert_eq!(card.pid, Some(pid));
    assert_eq!(card.port, Some(port));
    assert_eq!(card.state_word(), "running");
    assert!(harness.status().serving, "the project's URL answers");

    // 5. `lambo down` stops exactly that process.
    let report = harness.down();
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name == SERVER && step.detail.contains(&pid.to_string())),
        "the report must name the process it stopped: {:?}",
        report.steps
    );

    // 6. The listener is gone, the process is gone, and nothing claims to be
    //    running.
    expect_released(&harness, "the service to release its port");
    wait_until(&harness, "the process to exit", || {
        !process::is_running(pid, harness.os)
    });
    wait_until(&harness, "status to report it stopped", || {
        !harness.card().running
    });
    assert!(harness.card().pid.is_none());
}

#[test]
fn the_port_is_reusable_across_repeated_lifecycles() {
    // A port that stays bound after `down` is the classic way a lifecycle test
    // passes once and fails the second time.
    let harness = Harness::new(free_port());
    let port = harness.port();

    for round in 1..=3 {
        harness.up();
        wait_until(&harness, &format!("round {round} to serve"), || {
            port::is_listening(port)
        });
        har_request(&harness, round);

        let pid = harness.pid().expect("a pid while running");
        harness.down();
        expect_released(&harness, &format!("round {round} to release the port"));
        wait_until(
            &harness,
            &format!("round {round}'s process to exit"),
            || !process::is_running(pid, harness.os),
        );
    }
}

/// One HTTP round, so a failure names the round it happened in.
fn har_request(harness: &Harness, round: u8) {
    let url = harness.url();
    let response = http_get(&url).unwrap_or_else(|error| panic!("round {round}: {error}"));
    assert_eq!(response.0, 200, "round {round}: {response:?}");
    assert_eq!(response.1, FIXTURE_BODY, "round {round}");
}

#[test]
fn restart_brings_the_server_back_with_a_new_process() {
    let harness = Harness::new(free_port());
    harness.up();
    wait_until(&harness, "the first server to bind", || {
        port::is_listening(harness.port())
    });
    let first = harness.pid().expect("a pid");

    harness.down();
    expect_released(&harness, "the first server to stop");

    // Starting again is what a user does, and it has to work: a stopped service
    // is not a service that can never start again.
    harness.up();
    wait_until(&harness, "the second server to bind", || {
        port::is_listening(harness.port())
    });
    let second = harness.pid().expect("a pid after the restart");
    assert_ne!(first, second, "a restart must be a new process");
    expect_serving(&harness, "the restarted server to answer");
}

#[test]
fn a_server_that_exits_immediately_is_never_claimed_as_running() {
    // A service that starts and dies - a broken configuration, a missing
    // library - must never be reported as running. The engine watches the
    // process it started, so the card goes back to stopped on its own, and
    // nothing afterwards may claim otherwise.
    let harness = Harness::new(free_port());
    fs::write(harness.docroot().join("fixture-exit"), "1").unwrap();

    // `up` says so rather than reporting a start that did not happen.
    let error = harness
        .try_up()
        .expect_err("a server that dies on start-up must not be reported as started");
    assert!(
        error.to_string().contains(SERVER),
        "the failure must name what failed: {error}"
    );

    wait_until(&harness, "the process to exit", || !harness.card().running);
    assert!(
        !port::is_listening(harness.port()),
        "a server that died must not hold its port"
    );
    assert!(harness.card().pid.is_none());
    assert!(!harness.status().serving, "a dead server serves nothing");
    // The engine narrates the start, and the server it belongs to is named in
    // the line, which is what a user has to go on when nothing else says
    // anything.
    let lines = harness.log.lines();
    assert!(
        lines.iter().any(|line| line.contains(SERVER)),
        "the engine must have something to say about it: {lines:?}"
    );
}

#[test]
fn a_second_up_leaves_a_running_service_alone() {
    // `up` twice is a user re-running the command, and the answer is the
    // engine's own: the service is already running, so nothing is started,
    // nothing is killed, and the process that serves keeps serving. This is the
    // engine's `already running (pid N)`, which is the original's own line.
    let harness = Harness::new(free_port());
    harness.up();
    wait_until(&harness, "the service to bind", || {
        port::is_listening(harness.port())
    });
    let first = harness.pid().expect("a pid");

    let report = harness.up();

    assert_eq!(
        harness.pid(),
        Some(first),
        "a second `up` must not replace the process"
    );
    assert!(port::is_listening(harness.port()));
    assert!(harness.card().running);
    assert!(harness.status().serving);
    assert!(
        harness
            .log
            .lines()
            .iter()
            .any(|line| line.contains("already running")),
        "the engine says why nothing happened: {:?}",
        harness.log.lines()
    );
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name == SERVER && step.detail.contains("already running")),
        "and the report carries it: {:?}",
        report.steps
    );

    harness.down();
}

#[test]
fn a_server_that_starts_slowly_but_stays_alive_becomes_healthy() {
    // A slow start-up is not a dead process: the engine holds the process, and
    // the port appears when the server is ready for it.
    let harness = Harness::new(free_port());
    fs::write(harness.docroot().join("fixture-slow"), "1500").unwrap();

    harness.up();
    let pid = harness.pid();
    assert!(pid.is_some(), "the process is alive from the start");
    expect_serving(&harness, "the slow server to answer");
    assert!(harness.card().running, "and it is still the same service");
    assert_eq!(harness.card().pid, pid);
}

#[test]
fn a_server_that_binds_but_never_answers_is_not_reported_as_serving() {
    // The distinction the whole product rests on: a listener is not a server.
    let harness = Harness::new(free_port());
    fs::write(harness.docroot().join("fixture-silent"), "1").unwrap();

    // `up` waits for the project's own URL, so a port that never answers is a
    // failure that names the wait - not a project reported as up.
    let error = harness
        .try_up()
        .expect_err("a server that never answers must not be reported as up");
    assert!(
        error.to_string().contains(SERVER),
        "the failure names what never became healthy: {error}"
    );

    // The process is alive and holds the port, so the service is running...
    wait_until(&harness, "the silent server to bind", || {
        port::is_listening(harness.port())
    });
    assert!(harness.card().running);
    // ...and the project's URL does not answer, so nothing claims it serves.
    assert!(
        !harness.status().serving,
        "a port that never answers is not a serving project"
    );
    assert!(
        http_get(&harness.url()).is_err(),
        "there is no HTTP response to be had"
    );

    // A silent server is a real process, and has to be stopped like one.
    harness.down();
}

#[test]
fn an_occupied_port_fails_the_start_and_says_which_port() {
    let harness = Harness::new(free_port());
    let port = harness.port();
    // Something else - not Lambo - holds the port.
    let occupant = TcpListener::bind(("127.0.0.1", port)).expect("the test holds the port");

    let error = harness
        .try_up()
        .expect_err("a port in use is a failure, not a silent fallback");
    assert!(
        error.to_string().contains(&port.to_string()),
        "the failure must name the port it could not have: {error}"
    );
    assert!(!harness.card().running);
    assert!(harness.card().pid.is_none());

    drop(occupant);
    // And the service starts once the port is free, which is the point of
    // reporting it rather than moving to another port.
    harness.up();
    wait_until(
        &harness,
        "the service to bind once the port is free",
        || port::is_listening(port),
    );
}

#[test]
fn down_stops_only_what_lambo_started() {
    let harness = Harness::new(free_port());

    // A process of the same program, started by hand and living *outside* the
    // installation's bin directory: Lambo must not touch it.
    let outside = harness.temp.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let unrelated_exe = outside.join(harness.os.executable_name("not-lambos-fixture"));
    fs::copy(fixture_binary(), &unrelated_exe).unwrap();
    make_executable(&unrelated_exe);
    let unrelated_port = free_port();
    let child = ChildGuard::spawn(&unrelated_exe, unrelated_port, &harness.docroot());
    let unrelated_pid = child.pid();
    wait_until(&harness, "the unrelated process to bind", || {
        port::is_listening(unrelated_port)
    });

    harness.up();
    wait_until(&harness, "our service to bind", || {
        port::is_listening(harness.port())
    });

    harness.down();

    expect_released(&harness, "our service to stop");
    assert!(
        port::is_listening(unrelated_port),
        "a process Lambo did not start must survive `down`"
    );
    assert!(process::is_running(unrelated_pid, harness.os));
}

#[test]
fn the_sweep_cleans_up_what_a_previous_run_left_behind() {
    // The start-up sweep is the answer to a crash: a process from the last run
    // is still holding a port, and the next start has to get rid of it. It lives
    // in the stack, so a test can drive it exactly as the window does.
    let harness = Harness::new(free_port());

    // A leftover of this installation: its executable is under `bin/`.
    let leftover_port = free_port();
    let exe = harness
        .temp
        .join("bin/fixture")
        .join(harness.os.executable_name("lambo-fixture-server"));
    let leftover = ChildGuard::spawn(&exe, leftover_port, &harness.docroot());
    let leftover_pid = leftover.pid();
    wait_until(&harness, "the leftover to bind", || {
        port::is_listening(leftover_port)
    });

    // Our own service, running.
    harness.up();
    wait_until(&harness, "the service to bind", || {
        port::is_listening(harness.port())
    });
    let ours = harness.pid().expect("a pid");

    let stack = Stack::build(
        harness.paths.root(),
        &PanelConfig::load(harness.paths.root()).expect("the configuration loads"),
        Arc::new(HostService::new()),
        harness.log.function(),
    );
    let killed = stack.sweep();

    assert!(
        killed
            .iter()
            .any(|line| line.contains(&leftover_pid.to_string())),
        "the sweep must report the leftover: {killed:?} / {:?}",
        harness.log.lines()
    );
    wait_until(&harness, "the leftover to be gone", || {
        !process::is_running(leftover_pid, harness.os)
    });
    // And our own service is untouched.
    assert!(
        harness.card().running,
        "the sweep must keep what is running"
    );
    assert_eq!(harness.pid(), Some(ours));
    assert!(process::is_running(ours, harness.os));

    harness.down();
}

#[test]
fn every_surface_resolves_the_same_url() {
    let harness = Harness::new(free_port());
    harness.up();
    wait_until(&harness, "the service to bind", || {
        port::is_listening(harness.port())
    });
    expect_serving(&harness, "the service to answer");

    let mut context = harness.context();
    let project_url = session::effective_url(
        &harness.paths,
        &harness.project,
        &harness.config,
        harness.os,
    );
    let bound = session::active_http_port(&harness.paths, harness.os);
    let status = session::status(Some(&harness.project), &mut context).expect("status");

    assert_eq!(bound, Some(harness.port()));
    assert_eq!(project_url, harness.url());
    assert_eq!(status.url.as_deref(), Some(project_url.as_str()));
    assert!(status.serving);

    harness.down();
}

// ---------------------------------------------------------------------------
// Framework projects
// ---------------------------------------------------------------------------

/// A Lambo home with the installation document already written.
///
/// The panel document is where a scaffolded project and its virtual host are
/// registered, so the two project tests below need nothing else: no service, no
/// project file, no catalogue.
fn empty_home() -> TempDir {
    let temp = TempDir::new();
    Paths::from_root(temp.path())
        .ensure_layout()
        .expect("the installation layout");
    PanelConfig::default_config()
        .save(temp.path())
        .expect("the installation's configuration is written");
    temp
}

#[test]
fn a_scaffolded_project_is_registered_and_published_on_its_domain() {
    let temp = empty_home();
    let home = temp.path();

    // The two files the registration writes. A test must never touch the
    // machine's own hosts file, so both are redirected into the fixture.
    let hosts = temp.join("hosts");
    let include = temp.join("conf/apache/vhosts.conf");
    let mut document = PanelConfig::load(home).expect("the configuration loads");
    document.settings.hosts_file = hosts.display().to_string();
    document.settings.apache_vhosts_include = include.display().to_string();
    document.save(home).expect("the settings are written");

    let log = Log::default();
    let created = session::create_project(home, "Static HTML", "My Shop!", "", log.function())
        .expect("the project is created");

    assert_eq!(created.name, "my-shop", "the name is slugified");
    assert_eq!(created.framework, "Static HTML");
    assert_eq!(
        created.domain, "my-shop.test",
        "an empty domain is the name"
    );
    assert_eq!(created.proxy_port, 0, "a static project has no dev server");
    assert_eq!(created.warning, None);

    // The scaffold: one file, in `www/`, named after the project.
    let root = home.join("www").join("my-shop");
    assert_eq!(created.doc_root, root);
    let html = fs::read_to_string(root.join("index.html")).expect("index.html");
    assert!(html.contains("<title>my-shop</title>"), "{html}");
    assert!(html.contains("served by Lambo PHP"), "{html}");

    // The registration: the project and its vhost, in the same document.
    let document = PanelConfig::load(home).expect("the configuration loads");
    assert_eq!(document.projects.len(), 1, "one project");
    let project = &document.projects[0];
    assert_eq!(project.name, "my-shop");
    assert_eq!(project.framework, "Static HTML");
    assert_eq!(project.domain, "my-shop.test");
    assert_eq!(project.docroot, root.display().to_string());
    assert_eq!(project.port, 0);

    let vhost = document
        .vhosts
        .iter()
        .find(|vhost| vhost.domain == "my-shop.test")
        .expect("the vhost is registered");
    assert_eq!(vhost.docroot, "{base}/www/my-shop");
    assert_eq!(vhost.port, 80);
    assert_eq!(vhost.server_type, "apache");
    assert!(vhost.enabled, "a new project is published");
    assert_eq!(vhost.proxy_port, 0);

    // The publication: the domain in the hosts file, and the vhost in the
    // Apache include the server reads.
    let hosts_text = fs::read_to_string(&hosts).expect("the hosts file");
    assert!(hosts_text.contains("my-shop.test"), "{hosts_text}");
    let include_text = fs::read_to_string(&include).expect("the Apache include");
    assert!(include_text.contains("my-shop.test"), "{include_text}");
    assert!(include_text.contains("www/my-shop"), "{include_text}");

    // And what the user is told, in the original's words.
    let lines = log.lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("created index.html boilerplate")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("http://my-shop.test")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("Restart Stack")),
        "{lines:?}"
    );
}

#[test]
fn creating_a_project_reports_what_is_missing_or_unknown() {
    let temp = empty_home();
    let home = temp.path();
    let log = Log::default();

    let error = session::create_project(home, "", "app", "", log.function())
        .expect_err("a framework is required");
    assert_eq!(error.to_string(), "projects: pick a framework first");

    let error = session::create_project(home, "Docker Compose", "app", "", log.function())
        .expect_err("the framework is unknown");
    assert_eq!(
        error.to_string(),
        "projects: unknown framework Docker Compose"
    );

    let error = session::create_project(home, "Static HTML", "!!!", "", log.function())
        .expect_err("a name of punctuation slugifies to nothing");
    assert_eq!(error.to_string(), "projects: project name required");

    // Nothing was scaffolded and nothing was registered.
    assert!(
        !home.join("www").join("app").exists(),
        "no project directory"
    );
    let document = PanelConfig::load(home).expect("the configuration loads");
    assert!(document.projects.is_empty());
}

#[test]
fn deleting_a_project_takes_its_directory_its_registration_and_its_domain_with_it() {
    let temp = empty_home();
    let home = temp.path();
    let hosts = temp.join("hosts");
    let include = temp.join("conf/apache/vhosts.conf");
    let mut document = PanelConfig::load(home).expect("the configuration loads");
    document.settings.hosts_file = hosts.display().to_string();
    document.settings.apache_vhosts_include = include.display().to_string();
    document.save(home).expect("the settings are written");

    let log = Log::default();
    session::create_project(home, "Static HTML", "my-shop", "", log.function())
        .expect("the project is created");
    let root = home.join("www").join("my-shop");
    assert!(root.join("index.html").exists());
    assert!(
        fs::read_to_string(&hosts)
            .expect("the hosts file")
            .contains("my-shop.test")
    );

    assert!(
        session::delete_project(home, "my-shop", log.function())
            .expect("nothing about deleting fails"),
        "there was a project to delete"
    );
    assert!(!root.exists(), "the project directory is gone");

    let document = PanelConfig::load(home).expect("the configuration loads");
    assert!(document.projects.is_empty(), "the registration is gone");
    assert!(
        document
            .vhosts
            .iter()
            .all(|vhost| vhost.domain != "my-shop.test"),
        "the vhost is gone"
    );
    let hosts_text = fs::read_to_string(&hosts).expect("the hosts file");
    assert!(!hosts_text.contains("my-shop.test"), "{hosts_text}");
    let include_text = fs::read_to_string(&include).expect("the Apache include");
    assert!(!include_text.contains("my-shop.test"), "{include_text}");

    // Deleting it again is not an error; there is simply nothing to delete.
    assert!(
        !session::delete_project(home, "my-shop", log.function())
            .expect("nothing fails either way"),
        "the second deletion has nothing to do"
    );
}

#[test]
fn a_project_can_move_to_another_domain_without_its_files_moving() {
    let temp = empty_home();
    let home = temp.path();
    let hosts = temp.join("hosts");
    let include = temp.join("conf/apache/vhosts.conf");
    let mut document = PanelConfig::load(home).expect("the configuration loads");
    document.settings.hosts_file = hosts.display().to_string();
    document.settings.apache_vhosts_include = include.display().to_string();
    document.save(home).expect("the settings are written");

    let log = Log::default();
    session::create_project(home, "Static HTML", "my-shop", "", log.function())
        .expect("the project is created");
    let root = home.join("www").join("my-shop");

    assert!(
        session::set_project_domain(home, "my-shop", "shop.lan", log.function())
            .expect("the domain change is published"),
        "there was a project to move"
    );

    // The three places the change has to reach, all through the one call.
    let document = PanelConfig::load(home).expect("the configuration loads");
    assert_eq!(document.projects[0].domain, "shop.lan");
    assert!(
        document
            .vhosts
            .iter()
            .any(|vhost| vhost.domain == "shop.lan"),
        "the vhost followed the project"
    );
    assert!(
        document
            .vhosts
            .iter()
            .all(|vhost| vhost.domain != "my-shop.test"),
        "and the old domain is gone"
    );
    let hosts_text = fs::read_to_string(&hosts).expect("the hosts file");
    assert!(hosts_text.contains("127.0.0.1 shop.lan"), "{hosts_text}");
    assert!(!hosts_text.contains("my-shop.test"), "{hosts_text}");
    let include_text = fs::read_to_string(&include).expect("the Apache include");
    assert!(
        include_text.contains("ServerName shop.lan"),
        "{include_text}"
    );

    // The files stayed where they were: a domain is a name, not a directory.
    assert!(
        root.join("index.html").exists(),
        "the project is still there"
    );

    // A project that is not registered is not an error, it is an answer.
    assert!(
        !session::set_project_domain(home, "gone", "gone.test", log.function())
            .expect("nothing fails"),
        "there is no such project"
    );
}

#[test]
fn the_virtual_host_table_is_edited_and_published_through_the_shared_core() {
    let temp = empty_home();
    let home = temp.path();
    let hosts = temp.join("hosts");
    let include = temp.join("conf/apache/vhosts.conf");
    let sites = temp.join("conf/nginx/sites");
    let mut document = PanelConfig::load(home).expect("the configuration loads");
    document.settings.hosts_file = hosts.display().to_string();
    document.settings.apache_vhosts_include = include.display().to_string();
    document.settings.nginx_sites_dir = sites.display().to_string();
    // A default document ships one sample host - the original's own `myapp.test`,
    // disabled - so the table starts with a row the user never added. It is
    // asserted here and then cleared: the rules below are about the rows a user
    // creates, and counting the sample one would hide a duplicate row.
    assert_eq!(
        document.vhosts.len(),
        1,
        "the default document ships exactly the sample host"
    );
    assert_eq!(document.vhosts[0].domain, "myapp.test");
    assert!(!document.vhosts[0].enabled, "and it ships switched off");
    document.vhosts.clear();
    document.save(home).expect("the settings are written");

    let log = Log::default();

    // A rejected form carries the page's own message, and says so in the log.
    let refused = session::save_vhost(
        home,
        None,
        &VhostForm {
            name: "  ".to_owned(),
            extension: ".test".to_owned(),
            port: "80".to_owned(),
            server: "apache".to_owned(),
            docroot: "{base}/www/shop".to_owned(),
        },
        log.function(),
    )
    .expect_err("a domain is required");
    assert_eq!(refused.to_string(), "domain name is required");
    assert!(
        log.lines()
            .iter()
            .any(|line| line == "vhost save: domain name is required"),
        "{:?}",
        log.lines()
    );

    // An accepted form is stored, on by default, and is what the table lists.
    let saved = session::save_vhost(
        home,
        None,
        &VhostForm {
            name: "shop".to_owned(),
            extension: String::new(),
            port: "8080".to_owned(),
            server: "both".to_owned(),
            docroot: "  {base}/www/shop  ".to_owned(),
        },
        log.function(),
    )
    .expect("the vhost is saved");
    assert_eq!(saved.domain, "shop.test", "an empty extension is .test");
    assert_eq!(saved.docroot, "{base}/www/shop", "the field is trimmed");
    assert_eq!(saved.port, 8080);
    assert_eq!(saved.server_type, "both");
    assert!(saved.enabled, "a new host is enabled");
    assert_eq!(session::vhosts(home).expect("the table loads").len(), 1);

    // Saving edits the document; only applying writes the system files.
    assert!(!hosts.exists(), "saving publishes nothing");

    session::apply_vhosts(home, log.function()).expect("the document is published");
    let hosts_text = fs::read_to_string(&hosts).expect("the hosts file");
    assert!(hosts_text.contains("127.0.0.1 shop.test"), "{hosts_text}");
    let include_text = fs::read_to_string(&include).expect("the Apache include");
    assert!(
        include_text.contains("ServerName shop.test"),
        "{include_text}"
    );
    assert!(
        sites.join("lambo-shop.test.conf").is_file(),
        "both servers are served, so both files exist"
    );
    assert!(
        log.lines()
            .iter()
            .any(|line| line.starts_with("vhosts applied")),
        "{:?}",
        log.lines()
    );

    // Editing keeps the flag of the row it replaces and keeps nothing else.
    let mut form = VhostForm::from_vhost(&saved);
    assert_eq!(form.port, "8080");
    form.port = "80".to_owned();
    let edited = session::save_vhost(home, Some("shop.test"), &form, log.function())
        .expect("the edit is saved");
    assert_eq!(edited.port, 80);
    assert!(edited.enabled);
    assert_eq!(session::vhosts(home).expect("the table loads").len(), 1);

    // Deleting removes the row; the second deletion has nothing to do.
    assert!(
        session::delete_vhost(home, "shop.test", log.function()).expect("nothing fails"),
        "the host was there"
    );
    assert!(
        !session::delete_vhost(home, "shop.test", log.function()).expect("nothing fails"),
        "and now it is not"
    );

    session::apply_vhosts(home, log.function()).expect("the removal is published");
    let hosts_text = fs::read_to_string(&hosts).expect("the hosts file");
    assert!(!hosts_text.contains("shop.test"), "{hosts_text}");
    assert!(
        !sites.join("lambo-shop.test.conf").exists(),
        "the generated site is swept away"
    );
    let include_text = fs::read_to_string(&include).expect("the Apache include");
    assert!(!include_text.contains("shop.test"), "{include_text}");
}
