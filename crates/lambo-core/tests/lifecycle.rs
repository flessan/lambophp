//! The localhost service lifecycle, verified end to end.
//!
//! This is the acceptance criterion for Lambo's core promise:
//!
//! ```text
//! lambo up  →  a TCP listener exists  →  an HTTP request succeeds
//!           →  lambo status says running  →  lambo down
//!           →  the listener is gone  →  the process is gone  →  state is clean
//! ```
//!
//! Every step is observed, not inferred. A test that asserted only "a process
//! was spawned" would pass just as happily against a server that never bound a
//! port, and that is precisely the failure Lambo must never report as success.
//!
//! # What is a fixture and what is real
//!
//! Real PHP, Apache and MariaDB artifacts cannot be downloaded in this
//! environment, so the server process is `lambo-fixture-server`: a small
//! deterministic HTTP server that accepts the same arguments as `php -S`.
//! Because the command line matches, it is installed as the `php` executable of
//! a runtime and the **production** orchestration drives it unchanged -
//! `session::up`, `php::serve_spec`, `process::spawn`, `http::wait_until_up`,
//! `State`, and `session::down` are all the real code paths. Nothing here
//! reimplements the lifecycle.
//!
//! What this therefore does *not* prove is that real Apache or PHP behaves this
//! way. It proves Lambo's supervision of a managed child process is correct:
//! start, health-check, record, report, stop, and leave nothing behind.

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lambo_core::catalog::Catalog;
use lambo_core::config::{Config, DatabaseKind, ServerKind};
use lambo_core::download::LocalDownloader;
use lambo_core::http;
use lambo_core::paths::Paths;
use lambo_core::platform::{Os, Platform};
use lambo_core::port;
use lambo_core::process::ProcessSpec;
use lambo_core::project::Project;
use lambo_core::runtime::{self, RuntimeKind};
use lambo_core::session::{self, Context};
use lambo_core::state::State;

/// The body the fixture server returns.
const FIXTURE_BODY: &str = "Lambo PHP fixture OK";

/// How long a test waits for a server to start or a port to be released.
///
/// Generous enough for a loaded CI machine, short enough that a hang is
/// reported rather than endured.
const WAIT: Duration = Duration::from_secs(20);

/// How often to poll while waiting.
const POLL: Duration = Duration::from_millis(50);

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

/// An isolated Lambo home, a project inside it, and a fixture runtime.
struct Harness {
    temp: TempDir,
    paths: Paths,
    project: Project,
    config: Config,
    os: Os,
}

impl Harness {
    /// Builds a home with the fixture installed as PHP `8.4.2` and selected.
    fn new(port: u16) -> Self {
        let temp = TempDir::new();
        let paths = Paths::from_root(temp.path());
        paths.ensure_layout().expect("layout");
        let os = Os::host();

        install_fixture_php(&paths, "8.4.2", os);

        // A plain-PHP project: document root is the project itself, no
        // database, and the PHP built-in server rather than Apache (which has
        // no artifact available here).
        let root = temp.join("shop");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("index.php"), "<?php echo 'hi';\n").unwrap();
        fs::write(
            root.join("lambo.yml"),
            "server:\n  kind: php\n  document_root: .\ndatabase:\n  kind: none\n",
        )
        .unwrap();

        let mut config = Config::default();
        config.server.kind = ServerKind::Php;
        config.server.port = port;
        config.database.kind = DatabaseKind::None;

        Self {
            temp,
            paths,
            project: Project::load(&root).expect("project loads"),
            config,
            os,
        }
    }

    fn context(&self) -> Context<'static> {
        Context {
            paths: self.paths.clone(),
            config: self.config.clone(),
            catalog: Catalog::embedded().expect("catalogue"),
            platform: Platform::new(self.os, lambo_core::platform::Arch::X86_64),
            // file:// only, so nothing in a test can reach the network.
            downloader: &LocalDownloader,
            os: self.os,
        }
    }

    /// The URL `lambo` would report and open.
    fn url(&self) -> String {
        self.project.url(&self.config)
    }

    fn port(&self) -> u16 {
        self.project.http_port(&self.config)
    }

    /// Everything worth knowing when an assertion fails.
    ///
    /// A lifecycle test that fails with "assertion failed" is useless; the
    /// interesting facts are whether the process lived, what it printed, and
    /// what Lambo recorded about it.
    fn diagnostics(&self, context: &str) -> String {
        let mut out = format!("--- diagnostics ({context}) ---\n");
        out.push_str(&format!("home: {}\n", self.paths.root().display()));
        out.push_str(&format!("url: {}\n", self.url()));
        out.push_str(&format!(
            "port listening: {}\n",
            port::is_listening(self.port())
        ));
        match State::load(&self.paths) {
            Ok(state) => {
                out.push_str(&format!("recorded services: {}\n", state.services.len()));
                for record in state.services.values() {
                    out.push_str(&format!(
                        "  {} pid={} alive={} port={:?} command={}\n",
                        record.name,
                        record.pid,
                        record.is_alive(self.os),
                        record.port,
                        record.command
                    ));
                }
            }
            Err(error) => out.push_str(&format!("state failed to load: {error}\n")),
        }
        let log = lambo_core::logs::file(&self.paths, lambo_core::logs::Group::Php, "server.log");
        match fs::read_to_string(&log) {
            Ok(text) => out.push_str(&format!("log {}:\n{text}\n", log.display())),
            Err(error) => out.push_str(&format!("log {}: {error}\n", log.display())),
        }
        out
    }
}

/// Installs the fixture server as the `php` executable of a runtime.
///
/// The fixture accepts `-S <addr> -t <docroot>`, exactly what
/// [`lambo_core::php::serve_spec`] builds, so the production start path drives
/// it without modification.
fn install_fixture_php(paths: &Paths, version: &str, os: Os) {
    let root = paths.runtime_version_dir(RuntimeKind::Php, version);
    fs::create_dir_all(&root).expect("runtime directory");
    let target = root.join(os.executable_name("php"));
    fs::copy(fixture_binary(), &target).expect("fixture is copied into the runtime");
    make_executable(&target);
    runtime::set_active(paths, RuntimeKind::Php, version).expect("fixture runtime selected");
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
/// `first_free` answers "is it free *now*", and two tests asking in parallel
/// can both be told yes about the same port, after which one server fails to
/// bind and the failure has nothing to do with the code under test. Each test
/// also checks the port is genuinely unused before it starts.
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
fn wait_until(harness: &Harness, context: &str, predicate: impl Fn() -> bool) {
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

// ---------------------------------------------------------------------------
// The acceptance test
// ---------------------------------------------------------------------------

#[test]
fn up_serves_http_and_down_leaves_nothing_behind() {
    let harness = Harness::new(free_port());
    let mut context = harness.context();
    let port = harness.port();
    let url = harness.url();

    // Nothing is listening before `up`.
    assert!(
        !port::is_listening(port),
        "port {port} is occupied before the test even starts"
    );

    // --- up ---------------------------------------------------------------
    let report = session::up(&harness.project, &mut context, false).unwrap_or_else(|error| {
        panic!(
            "`up` failed: {error}\ndetails: {:?}\n{}",
            error.details(),
            harness.diagnostics("up failed")
        )
    });
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.outcome != session::StepOutcome::Failed),
        "a step failed: {}",
        report.render()
    );
    assert_eq!(report.url.as_deref(), Some(url.as_str()));

    // --- a real listener exists -------------------------------------------
    wait_until(&harness, "the port to accept connections", || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });

    // --- a real HTTP request succeeds -------------------------------------
    let (status, body) = http_get(&url).unwrap_or_else(|error| {
        panic!(
            "the server is listening but did not answer {url}: {error}\n{}",
            harness.diagnostics("no HTTP response")
        )
    });
    assert_eq!(status, 200, "expected HTTP 200 from the fixture server");
    assert_eq!(
        body, FIXTURE_BODY,
        "the body must come from the fixture server"
    );

    // --- status reports it as running, and says so because it answers ------
    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert!(status.serving, "status must report the URL as serving");
    assert_eq!(status.url.as_deref(), Some(url.as_str()));

    let server = status
        .services
        .iter()
        .find(|service| service.name == lambo_core::state::names::PHP_SERVER)
        .expect("the PHP server is listed");
    assert!(server.recorded, "the service must be recorded");
    assert!(server.running, "the service must be running");
    assert_eq!(server.port, Some(port));
    let pid = server.pid.expect("a running service has a pid");
    assert!(
        lambo_core::process::is_running(pid, harness.os),
        "pid {pid} is recorded but not alive"
    );
    assert!(
        server.uptime.is_some(),
        "a running service reports how long it has been up"
    );

    // --- down --------------------------------------------------------------
    let report = session::down(&mut context).expect("down");
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.outcome != session::StepOutcome::Failed),
        "`down` reported a failure: {}",
        report.render()
    );

    // The endpoint stops answering. This is the assertion that matters: a PID
    // disappearing from the state file is not the same as the server stopping.
    wait_until(&harness, "the port to stop accepting connections", || {
        TcpStream::connect(("127.0.0.1", port)).is_err()
    });
    assert!(
        http_get(&url).is_err(),
        "HTTP still answers on {url} after `down`"
    );

    // --- no orphan, no stale state -----------------------------------------
    assert!(
        !lambo_core::process::is_running(pid, harness.os),
        "the managed process {pid} is still alive after `down`"
    );
    assert!(
        !port::is_listening(port),
        "something is still listening on {port} after `down`"
    );

    let state = State::load(&harness.paths).expect("state loads");
    assert!(
        state.services.is_empty(),
        "service state still claims something is running: {:?}",
        state
            .services
            .values()
            .map(|record| record.name.clone())
            .collect::<Vec<_>>()
    );

    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert!(
        !status.serving,
        "status must no longer report the URL as serving"
    );
    assert!(
        status.services.iter().all(|service| !service.running),
        "status reports a running service after `down`"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle robustness
// ---------------------------------------------------------------------------

#[test]
fn the_port_is_reusable_across_repeated_lifecycles() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();
    let url = harness.url();

    for cycle in 1..=3 {
        let label = format!("cycle {cycle}");

        session::up(&harness.project, &mut context, false).unwrap_or_else(|error| {
            panic!(
                "{label}: `up` failed: {error}\n{}",
                harness.diagnostics(&label)
            )
        });
        wait_until(&harness, &format!("{label}: HTTP to answer"), || {
            http_get(&url).is_ok()
        });
        let (status, body) = http_get(&url).expect("response");
        assert_eq!((status, body.as_str()), (200, FIXTURE_BODY), "{label}");

        // Each cycle must start exactly one server, on a fresh pid.
        let state = State::load(&harness.paths).unwrap();
        assert_eq!(
            state.services.len(),
            1,
            "{label}: expected one service, found {:?}",
            state.services.keys().collect::<Vec<_>>()
        );
        let pid = state.get(lambo_core::state::names::PHP_SERVER).unwrap().pid;

        session::down(&mut context).expect("down");
        wait_until(&harness, &format!("{label}: port to be released"), || {
            !port::is_listening(port)
        });
        assert!(
            !lambo_core::process::is_running(pid, harness.os),
            "{label}: pid {pid} survived `down`"
        );

        // The same port must be free again before the next cycle binds it.
        assert!(port::is_free(port), "{label}: port {port} was not released");
    }

    // Nothing accumulates across cycles.
    let state = State::load(&harness.paths).unwrap();
    assert!(state.services.is_empty(), "{:?}", state.services.keys());
    assert!(
        !harness.temp.join("shop").join("8.4.2.installing").exists(),
        "a staging directory was left behind"
    );
}

#[test]
fn restart_brings_the_server_back_with_a_new_process() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();
    let url = harness.url();

    session::up(&harness.project, &mut context, false).expect("first up");
    wait_until(&harness, "HTTP to answer", || http_get(&url).is_ok());
    let first_pid = State::load(&harness.paths)
        .unwrap()
        .get(lambo_core::state::names::PHP_SERVER)
        .expect("recorded")
        .pid;

    // `lambo restart` is `down` then `up`; the CLI composes them, so the
    // composition is what is exercised here.
    session::down(&mut context).expect("restart: down");
    wait_until(&harness, "the port to be released", || {
        !port::is_listening(port)
    });
    session::up(&harness.project, &mut context, false).expect("restart: up");
    wait_until(&harness, "HTTP to answer again", || http_get(&url).is_ok());

    let second_pid = State::load(&harness.paths)
        .unwrap()
        .get(lambo_core::state::names::PHP_SERVER)
        .expect("recorded")
        .pid;

    assert!(
        !lambo_core::process::is_running(first_pid, harness.os),
        "the pre-restart process {first_pid} is still alive"
    );
    assert!(
        lambo_core::process::is_running(second_pid, harness.os),
        "the post-restart process {second_pid} is not alive"
    );
    // The pid is expected to differ, but the contract is identity, not
    // inequality: a reused pid would still be correct. Assert what matters.
    let (status, body) = http_get(&url).expect("response after restart");
    assert_eq!((status, body.as_str()), (200, FIXTURE_BODY));

    session::down(&mut context).expect("final down");
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn a_server_that_exits_immediately_fails_up_without_leaving_a_false_record() {
    let port = free_port();
    let harness = Harness::new(port);
    // The fixture refuses to start when this marker is in the document root.
    fs::write(harness.temp.join("shop").join("fixture-exit"), b"").unwrap();

    let mut context = harness.context();
    let started = Instant::now();
    let error = session::up(&harness.project, &mut context, false)
        .expect_err("`up` must fail when the server cannot start");
    let elapsed = started.elapsed();

    // The process is dead within milliseconds of being spawned. Waiting out
    // the 30-second HTTP bound for it is what made a failed `lambo up` look
    // frozen for half a minute. The ceiling is deliberately loose - it has to
    // survive a loaded CI machine - but it is far below the bound, so it does
    // prove the bound was not consumed.
    assert!(
        elapsed < Duration::from_secs(10),
        "a dead server should fail fast, but `up` took {elapsed:?}"
    );

    let message = error.to_string();
    assert!(
        message.contains("exited"),
        "the error should say the process exited: {message}"
    );
    // Not a timeout. A timeout says "we waited and it never came", which sends
    // the user looking for slowness when the real fact is that nothing was
    // ever going to answer.
    assert!(
        !message.contains("did not become healthy within"),
        "a dead process must not be reported as a timeout: {message}"
    );
    // The exit code is available here because the handle is still held, and it
    // is the single most useful fact for whoever has to fix the start-up.
    assert!(
        error
            .details()
            .iter()
            .any(|d| d.contains("exited with code 3")),
        "the error should carry the exit code: {:?}",
        error.details()
    );
    // The diagnostic has to point at a log that actually exists.
    let log = lambo_core::logs::file(&harness.paths, lambo_core::logs::Group::Php, "server.log");
    let details = error.details().join("\n");
    assert!(
        details.contains(&log.display().to_string()),
        "the error must name the log file: {details}"
    );
    assert!(log.exists(), "the log the error points at does not exist");
    assert!(
        fs::read_to_string(&log)
            .map(|text| text.contains("fixture-exit"))
            .unwrap_or(false),
        "the log should contain the fixture's own diagnostic"
    );

    // Nothing may be reported as running, and nothing may be left listening.
    assert!(
        !port::is_listening(port),
        "the failed server is listening on {port}"
    );
    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert!(!status.serving);
    assert!(
        status.services.iter().all(|service| !service.running),
        "a service is reported running after a failed `up`: {status:?}"
    );

    // And `down` on the wreckage is a no-op rather than an error.
    session::down(&mut context).expect("`down` after a failed `up` must not fail");
    assert!(State::load(&harness.paths).unwrap().services.is_empty());
}

#[test]
fn a_server_that_starts_slowly_but_stays_alive_still_becomes_healthy() {
    let harness = Harness::new(free_port());
    // Bind after a delay. The process is alive the whole time and is merely
    // not ready yet - which is the case a liveness check must keep polling
    // through rather than conclude is dead. This is the guarantee that makes
    // failing fast on a dead process safe.
    fs::write(harness.temp.join("shop").join("fixture-slow"), b"2000").unwrap();

    let mut context = harness.context();
    let report = session::up(&harness.project, &mut context, false).unwrap_or_else(|error| {
        panic!(
            "a slow start-up must still succeed: {error}\ndetails: {:?}",
            error.details()
        )
    });
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.outcome != session::StepOutcome::Failed),
        "a step failed: {}",
        report.render()
    );

    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert!(
        status.serving,
        "the slow server never became healthy: {status:?}"
    );

    // It must also be a real, stoppable service, not something the health
    // check merely declared up.
    let response = http::get(&harness.url(), Duration::from_secs(5)).expect("the site answers");
    assert!(response.body.contains(FIXTURE_BODY));
    session::down(&mut context).expect("down");
}

#[test]
fn a_server_that_binds_but_never_answers_is_not_reported_as_healthy() {
    let port = free_port();
    let harness = Harness::new(port);
    // The fixture accepts connections and sends nothing, which is exactly the
    // failure a listening-socket check would call success.
    fs::write(harness.temp.join("shop").join("fixture-silent"), b"").unwrap();

    let mut context = harness.context();
    let started = Instant::now();
    let error = session::up(&harness.project, &mut context, false)
        .expect_err("`up` must fail when the server binds but never serves");
    let elapsed = started.elapsed();
    assert!(
        error.to_string().contains("never answered"),
        "the error should say the server never answered: {error}"
    );
    // The counterpart to the fast-fail case: this process is *alive*, so the
    // bounded timeout still applies in full. Failing fast here would mean a
    // slow server is misreported as dead, which is worse than waiting. The
    // floor is well below the 30-second bound but far above anything a
    // fast-fail could produce, so it does distinguish the two paths.
    assert!(
        elapsed >= Duration::from_secs(10),
        "a live-but-unhealthy server should wait out the bound, gave up after {elapsed:?}"
    );

    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert!(
        !status.serving,
        "a server that never answers must not be reported as serving"
    );

    // The invocation that failed still started a process, and it recorded it
    // before the health check ran. That record is what makes the failure
    // recoverable: without it the process would be an orphan with nothing
    // pointing at it.
    let state = State::load(&harness.paths).expect("state loads");
    let started = state
        .get(lambo_core::state::names::PHP_SERVER)
        .expect("the process this invocation started is recorded");
    assert!(
        started.is_alive(harness.os),
        "the recorded process is not the one that is listening"
    );

    session::down(&mut context).expect("down");
    wait_until(&harness, "the port to be released", || {
        !port::is_listening(port)
    });
    assert!(
        !lambo_core::process::is_running(started.pid, harness.os),
        "`down` did not stop the process the failed `up` started"
    );
}

#[test]
fn an_occupied_web_port_falls_back_and_says_so() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();

    // Hold the configured port with a listener Lambo does not own.
    let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind");

    // `lambo up` no longer refuses here. Refusing to start a developer's
    // project because something else squats port 80 is a worse outcome than
    // serving on the next free port - provided the change is stated and the URL
    // given is the one that actually answers.
    let report = session::up(&harness.project, &mut context, false)
        .unwrap_or_else(|error| panic!("`up` should fall back, not fail: {error}"));

    let url = report.url.as_deref().expect("`up` reports a URL");
    assert_ne!(
        url,
        harness.url(),
        "the URL must be the fallback, not the squatted port"
    );
    assert!(
        !url.ends_with(&format!(":{port}")),
        "the URL must not point at the occupied port: {url}"
    );

    // The change must be explained in the report. A silent port change reads as
    // Lambo ignoring the configuration.
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.detail.contains(&port.to_string())),
        "the report must explain the port change: {:?}",
        report.steps
    );

    // And the claim must be true: something actually answers at that URL.
    assert!(
        TcpStream::connect((
            "127.0.0.1",
            url.rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .expect("the fallback URL carries a port")
        ))
        .is_ok(),
        "nothing is listening at the URL `up` reported: {url}"
    );

    // The squatter must survive: Lambo never kills a process it did not start.
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the unrelated listener was disturbed"
    );

    let _ = session::down(&mut context);
    drop(squatter);
}

// ---------------------------------------------------------------------------
// Partial failure and shutdown isolation
// ---------------------------------------------------------------------------

/// A long-lived process Lambo has no business touching.
fn unrelated_process() -> ProcessSpec {
    #[cfg(windows)]
    {
        ProcessSpec::new(r"C:\Windows\System32\ping.exe", "bystander")
            .arg("-n")
            .arg("60")
            .arg("127.0.0.1")
    }
    #[cfg(not(windows))]
    {
        ProcessSpec::new("sleep", "bystander").arg("60")
    }
}

#[test]
fn a_failed_up_does_not_disturb_a_service_that_was_already_running() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();

    // A service started by an earlier invocation - here the database manager,
    // on its own port. `lambo up` must leave it alone even when `up` fails.
    let ui_port = free_port();
    let ui_dir = harness.temp.join("dbui");
    fs::create_dir_all(&ui_dir).unwrap();
    let ui_spec = ProcessSpec::new(fixture_binary(), "dbui")
        .arg("-S")
        .arg(format!("127.0.0.1:{ui_port}"))
        .arg("-t")
        .arg(ui_dir.display().to_string())
        .detached();
    let ui_child = lambo_core::process::spawn(&ui_spec, harness.os).expect("dbui starts");
    let ui_pid = ui_child.id();
    drop(ui_child);

    let mut state = State::default();
    let mut record =
        lambo_core::state::ServiceRecord::new(lambo_core::state::names::DBUI, ui_pid, "dbui")
            .with_port(ui_port);
    if let Some(identity) = lambo_core::process::identity_settled(
        ui_pid,
        harness.os,
        &ui_spec.program,
        std::time::Duration::from_millis(500),
    ) {
        record = record.with_identity(&identity);
    }
    state.record(record);
    state.save(&harness.paths).unwrap();

    wait_until(&harness, "the pre-existing service to answer", || {
        TcpStream::connect(("127.0.0.1", ui_port)).is_ok()
    });

    // Now make this invocation fail at the server step.
    fs::write(harness.temp.join("shop").join("fixture-exit"), b"").unwrap();
    let error = session::up(&harness.project, &mut context, false)
        .expect_err("`up` must fail when the server cannot start");
    assert!(
        error.to_string().contains("exited")
            || error.to_string().contains("never answered")
            || error.to_string().contains("could not"),
        "unexpected failure: {error}"
    );

    // The service that was already running is untouched - still alive, still
    // recorded, still answering. A failed `up` is not a reason to take down
    // something the user started earlier and may be relying on.
    assert!(
        lambo_core::process::is_running(ui_pid, harness.os),
        "the pre-existing service was stopped by a failed `up`"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", ui_port)).is_ok(),
        "the pre-existing service stopped answering"
    );
    let state = State::load(&harness.paths).unwrap();
    assert!(
        state.get(lambo_core::state::names::DBUI).is_some(),
        "the pre-existing service's record was dropped"
    );

    // `down` is the explicit request to stop everything, and it does - both.
    session::down(&mut context).expect("down");
    wait_until(&harness, "the pre-existing service to stop", || {
        !lambo_core::process::is_running(ui_pid, harness.os)
    });
    assert!(State::load(&harness.paths).unwrap().services.is_empty());
}

#[test]
fn down_stops_only_what_lambo_started() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();
    let url = harness.url();

    // An unrelated long-lived process, running the whole time.
    let spec = unrelated_process();
    let child = lambo_core::process::spawn(&spec, harness.os).expect("bystander starts");
    let bystander = child.id();
    drop(child);
    assert!(lambo_core::process::is_running(bystander, harness.os));

    session::up(&harness.project, &mut context, false).expect("up");
    wait_until(&harness, "HTTP to answer", || http_get(&url).is_ok());
    let managed = State::load(&harness.paths)
        .unwrap()
        .get(lambo_core::state::names::PHP_SERVER)
        .expect("managed service")
        .pid;

    session::down(&mut context).expect("down");
    wait_until(&harness, "the managed process to exit", || {
        !lambo_core::process::is_running(managed, harness.os)
    });

    // The bystander is Lambo's neighbour, not its child. Nothing about `down`
    // may reach it - no group signals, no name matching, no sweeping.
    assert!(
        lambo_core::process::is_running(bystander, harness.os),
        "`lambo down` killed a process it did not start"
    );

    let _ = lambo_core::process::stop(
        bystander,
        harness.os,
        None,
        std::time::Duration::from_secs(5),
    );
}

// ---------------------------------------------------------------------------
// State reconciliation
// ---------------------------------------------------------------------------

#[test]
fn a_stale_record_is_reconciled_rather_than_believed() {
    let port = free_port();
    let harness = Harness::new(port);
    let mut context = harness.context();

    // Write a record for a process that does not exist, as a crash would leave.
    let mut state = State::default();
    state.record(
        lambo_core::state::ServiceRecord::new(
            lambo_core::state::names::PHP_SERVER,
            u32::MAX,
            "php -S 127.0.0.1:1 -t /nowhere",
        )
        .with_port(port),
    );
    state.save(&harness.paths).unwrap();

    let status = session::status(Some(&harness.project), &mut context).expect("status");
    let server = status
        .services
        .iter()
        .find(|service| service.name == lambo_core::state::names::PHP_SERVER)
        .unwrap();
    assert!(
        !server.running,
        "a record for a dead process must not be reported as running"
    );
    assert!(
        !status.serving,
        "a dead record must not make the URL look served"
    );

    // Reading status pruned it, so `up` is not blocked by the stale record.
    assert!(
        State::load(&harness.paths).unwrap().services.is_empty(),
        "the stale record was not pruned"
    );

    let url = harness.url();
    session::up(&harness.project, &mut context, false).unwrap_or_else(|error| {
        panic!(
            "`up` after a stale record failed: {error}\n{}",
            harness.diagnostics("up after stale record")
        )
    });
    wait_until(&harness, "HTTP to answer", || http_get(&url).is_ok());
    session::down(&mut context).expect("down");
}

// ---------------------------------------------------------------------------
// URL generation is a single source of truth (§7, §18)
// ---------------------------------------------------------------------------

#[test]
fn every_surface_resolves_the_same_url() {
    let harness = Harness::new(8080);
    let mut context = harness.context();

    // `up` reports it, `status` reports it, and `open` would navigate to it.
    // They must agree, because a user who reads one and is taken to another
    // concludes Lambo is broken.
    let expected = harness.url();
    assert_eq!(expected, "http://localhost:8080");

    let status = session::status(Some(&harness.project), &mut context).expect("status");
    assert_eq!(status.url.as_deref(), Some(expected.as_str()));

    // The browser target is validated by the same module that opens it.
    assert!(
        lambo_core::browser::is_safe_url(&expected),
        "{expected} must be a URL Lambo is willing to open"
    );

    // A different port yields a different URL from the same single source.
    let mut other = harness.config.clone();
    other.server.port = 9090;
    assert_eq!(harness.project.url(&other), "http://localhost:9090");
}
