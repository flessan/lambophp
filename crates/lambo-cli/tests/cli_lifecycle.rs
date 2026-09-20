//! The CLI driven end to end, the way a person drives it.
//!
//! The core lifecycle is proven in `lambo-core`'s own tests. What those cannot
//! show is that the *command line* is wired to it correctly: that `lambo init`
//! writes a usable project file, that `lambo up` exits zero and actually leaves
//! a server answering, that `lambo status` tells the truth before and after, and
//! that `lambo down` exits zero and really stops it.
//!
//! Every assertion here is made on a real child process running the real `lambo`
//! binary, with a real TCP connection in the middle.
//!
//! # Fixtures
//!
//! Production PHP, Apache and MariaDB artifacts cannot be downloaded in this
//! environment, so the server is `lambo-fixture-server` installed as the
//! project's PHP runtime, and the project is configured to use PHP's built-in
//! server with no database. The CLI, its argument parsing, its exit codes and
//! its output are all real; only the interpreter underneath is a stand-in.
//! Physical Windows behaviour and a real desktop browser remain manual smoke
//! tests - see `docs/windows.md`.

#[path = "support/capture.rs"]
mod capture;

use std::cell::Cell;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// How long to wait for the server to start or stop.
const WAIT: Duration = Duration::from_secs(30);

/// How often to poll.
const POLL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "lambo-cli-{}-{}-{}",
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

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The `lambo` binary under test.
fn lambo() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lambo"))
}

/// The fixture server, built alongside `lambo` by the same cargo invocation.
fn fixture() -> PathBuf {
    lambo().with_file_name(format!(
        "lambo-fixture-server{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// An isolated Lambo home, a project directory, and a fixture PHP runtime.
struct Harness {
    home: TempDir,
    project: TempDir,
    port: u16,
    invocation: Cell<usize>,
}

impl Harness {
    fn new(port: u16) -> Self {
        let fixture = fixture();
        assert!(
            fixture.is_file(),
            "the fixture server was not built at {}; run `cargo test --workspace --all-targets` \
             so every workspace binary exists",
            fixture.display()
        );

        let home = TempDir::new("home");
        let project = TempDir::new("project");

        // Install the fixture as PHP 8.4.2 and mark it active, which is the
        // state `lambo php install` would leave behind.
        let runtime = home.join("php/8.4.2");
        fs::create_dir_all(&runtime).expect("runtime directory");
        let executable = runtime.join(lambo_core::platform::Os::host().executable_name("php"));
        fs::copy(&fixture, &executable).expect("fixture installed as php");
        make_executable(&executable);
        fs::write(home.join("php/.active"), "8.4.2").expect("active marker");

        // A plain-PHP project: an entry point and nothing that implies a
        // framework.
        fs::write(project.join("index.php"), "<?php echo 'hello';\n").expect("index.php");

        Self {
            home,
            project,
            port,
            invocation: Cell::new(0),
        }
    }

    /// Runs `lambo` with this home, in the project directory.
    fn lambo(&self, args: &[&str]) -> Output {
        let invocation = self.invocation.get() + 1;
        self.invocation.set(invocation);
        let mut command = Command::new(lambo());
        command
            .args(args)
            .current_dir(&self.project.path)
            .env(lambo_core::paths::HOME_ENV, &self.home.path)
            // Never inherit a developer's own Lambo home or colour settings.
            .env("NO_COLOR", "1");
        capture::output(&mut command, WAIT).unwrap_or_else(|error| {
            panic!(
                "CLI invocation #{invocation}: `lambo {}` did not complete within {WAIT:?}\n\
                 project: {}\nhome: {}\n{error}",
                args.join(" "),
                self.project.path.display(),
                self.home.path.display(),
            )
        })
    }

    /// Runs a command and asserts it exited zero, showing its output if not.
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

    /// Runs a command and asserts it failed, returning stderr and stdout.
    fn expect_failure(&self, args: &[&str]) -> String {
        let output = self.lambo(args);
        assert!(
            !output.status.success(),
            "`lambo {}` was expected to fail but exited zero\n--- stdout ---\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }
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

/// A port no other test in this binary is using.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(46_200);

    for _ in 0..400 {
        let candidate = NEXT.fetch_add(1, Ordering::SeqCst);
        if candidate > 46_900 {
            NEXT.store(46_200, Ordering::SeqCst);
            continue;
        }
        if lambo_core::port::is_free(candidate) {
            return candidate;
        }
    }
    panic!("no free port could be allocated");
}

/// Polls until a probe returns a value, retaining its last error on failure.
fn wait_until<T>(
    what: &str,
    harness: &Harness,
    mut predicate: impl FnMut() -> Result<Option<T>, String>,
) -> T {
    let deadline = Instant::now() + WAIT;
    let mut last = "condition was not satisfied".to_owned();
    while Instant::now() < deadline {
        match predicate() {
            Ok(Some(value)) => return value,
            Ok(None) => last = "condition was not satisfied".to_owned(),
            Err(error) => last = error,
        }
        std::thread::sleep(POLL);
    }
    let status = harness.lambo(&["status"]);
    panic!(
        "timed out after {WAIT:?} waiting for {what}\nlast probe: {last}\n--- lambo status ---\n{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

/// A real HTTP GET, preserving the error as well as the status code and body.
fn http_get(url: &str) -> lambo_core::error::Result<(u16, String)> {
    lambo_core::http::get(url, Duration::from_secs(5))
        .map(|response| (response.status, response.body))
}

// ---------------------------------------------------------------------------
// The walkthrough
// ---------------------------------------------------------------------------

#[test]
fn init_up_status_down_status_through_the_command_line() {
    let port = free_port();
    let harness = Harness::new(port);
    let url = harness.url();

    // --- lambo init --------------------------------------------------------
    let out = harness.expect_success(&["init"]);
    assert!(
        harness.project.join("lambo.yml").is_file(),
        "`lambo init` did not write lambo.yml:\n{out}"
    );
    let written = fs::read_to_string(harness.project.join("lambo.yml")).unwrap();
    assert!(
        written.contains("document_root"),
        "the generated project file has no document root:\n{written}"
    );

    // Point the project at the PHP built-in server on a free port, with no
    // database - the configuration the fixture can actually serve.
    //
    // This edits lambo.yml rather than running `lambo config set`, because a
    // project's own file overrides the global configuration and `lambo init`
    // writes `server.kind: apache` into it. Setting the global value alone
    // would be ignored, which is exactly the trap the error messages now warn
    // about.
    let lambofile = harness.project.join("lambo.yml");
    let configured = format!(
        "name: {name}\nphp: stable\nserver:\n  kind: php\n  port: {port}\n  \
         document_root: .\ndatabase:\n  kind: none\n",
        name = "shop",
        port = port
    );
    fs::write(&lambofile, configured).expect("lambo.yml rewritten");

    // The CLI reads that file back and resolves the PHP server, not Apache.
    let out = harness.expect_success(&["status"]);
    assert!(
        out.contains("PHP built-in server") || out.to_lowercase().contains("php"),
        "status did not pick up the project's server kind:\n{out}"
    );

    // --- lambo status, before anything is up -------------------------------
    let out = harness.expect_success(&["status"]);
    assert!(
        out.contains("not answering"),
        "status claims the project is served before `up`:\n{out}"
    );
    assert!(
        !out.contains("php-server  running"),
        "status reports a running server before `up`:\n{out}"
    );

    // --- lambo up ----------------------------------------------------------
    // `--no-browser` because CI has no desktop to open one on.
    let out = harness.expect_success(&["up", "--no-browser"]);
    assert!(
        out.contains(&url),
        "`lambo up` did not report the project URL:\n{out}"
    );

    // The acceptance criterion, observed rather than inferred: a real TCP
    // connection returns a real HTTP response.
    let (status, body) = wait_until("the server to answer", &harness, || {
        http_get(&url).map(Some).map_err(|error| error.to_string())
    });
    assert_eq!(status, 200, "expected HTTP 200 from the fixture server");
    assert_eq!(body, "Lambo PHP fixture OK", "unexpected body");

    // --- lambo status, while it is up --------------------------------------
    let out = harness.expect_success(&["status"]);
    assert!(
        out.contains("answering") && !out.contains("not answering"),
        "status does not report the project as served:\n{out}"
    );
    assert!(
        out.contains("php-server") && out.contains("running"),
        "status does not report the server as running:\n{out}"
    );
    assert!(
        out.contains(&format!(":{port}")),
        "status does not report the port:\n{out}"
    );

    // --- lambo down --------------------------------------------------------
    harness.expect_success(&["down"]);

    // Not merely "the record is gone": the endpoint must stop answering and
    // the port must be released.
    wait_until("the server to stop answering", &harness, || {
        Ok(
            (http_get(&url).is_err() && TcpStream::connect(("127.0.0.1", port)).is_err())
                .then_some(()),
        )
    });

    // --- lambo status, after down ------------------------------------------
    let out = harness.expect_success(&["status"]);
    assert!(
        out.contains("not answering"),
        "status still reports the project as served after `down`:\n{out}"
    );
    assert!(
        !out.contains("php-server  running"),
        "status still reports a running server after `down`:\n{out}"
    );

    // The port is available to whoever asks next.
    assert!(
        lambo_core::port::is_free(port),
        "port {port} was not released by `lambo down`"
    );
}

// ---------------------------------------------------------------------------
// Exit codes and failure reporting
// ---------------------------------------------------------------------------

#[test]
fn a_command_run_outside_a_project_says_so_and_exits_zero() {
    let home = TempDir::new("home");
    let elsewhere = TempDir::new("elsewhere");

    // `lambo status` is the bare default and must work with no project: a user
    // who runs it in the wrong directory should be told, not crashed on.
    let output = Command::new(lambo())
        .arg("status")
        .current_dir(&elsewhere.path)
        .env(lambo_core::paths::HOME_ENV, &home.path)
        .env("NO_COLOR", "1")
        .output()
        .expect("lambo runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(output.status.success(), "`lambo status` failed:\n{stdout}");
    assert!(
        stdout.contains("no lambo.yml"),
        "status should say there is no project here:\n{stdout}"
    );
    assert!(
        stdout.contains("lambo init"),
        "status should name the command that fixes it:\n{stdout}"
    );
}

#[test]
fn up_fails_with_a_nonzero_exit_when_the_server_cannot_start() {
    let port = free_port();
    let harness = Harness::new(port);

    harness.expect_success(&["init"]);
    fs::write(
        harness.project.join("lambo.yml"),
        format!(
            "name: shop\nphp: stable\nserver:\n  kind: php\n  port: {port}\n  \
             document_root: .\ndatabase:\n  kind: none\n"
        ),
    )
    .expect("lambo.yml rewritten");

    // Make the server refuse to start.
    fs::write(harness.project.join("fixture-exit"), b"").unwrap();

    let combined = harness.expect_failure(&["up", "--no-browser"]);
    assert!(
        combined.contains("php"),
        "the failure should name the service that failed:\n{combined}"
    );

    // A failed `up` must not leave the impression that something is serving.
    let out = harness.expect_success(&["status"]);
    assert!(
        out.contains("not answering"),
        "status reports a served project after a failed `up`:\n{out}"
    );

    // And whatever did start is still stoppable.
    harness.expect_success(&["down"]);
    assert!(
        lambo_core::port::is_free(port),
        "port {port} is still held after `down`"
    );
}

#[test]
fn advice_about_switching_servers_names_the_file_that_actually_decides() {
    // A project's lambo.yml overrides the global configuration, and `lambo
    // init` writes `server.kind: apache` into it. So the only advice that can
    // work is to edit that file; telling the user to run `lambo config set`
    // sends them to change a value their own project then ignores.
    let advice = lambo_core::config::SWITCH_TO_PHP_SERVER;
    assert!(
        advice.contains("lambo.yml"),
        "the advice must name the project file that wins: {advice}"
    );
    assert!(
        advice.find("lambo.yml") < advice.find("lambo config set"),
        "the project file must come first, since it is the one that decides: {advice}"
    );

    // And the message a user actually hits carries that advice.
    let port = free_port();
    let harness = Harness::new(port);
    harness.expect_success(&["init"]);
    harness.expect_success(&["config", "set", "database.kind", "none"]);
    harness.expect_success(&["config", "set", "server.port", &port.to_string()]);

    // lambo.yml still says apache, and no Apache build exists for this
    // platform, so `up` fails with the advice in it.
    if cfg!(windows) {
        return; // Windows does have an Apache entry; the case does not arise.
    }
    let combined = harness.expect_failure(&["up", "--no-browser"]);
    assert!(
        combined.contains("lambo.yml"),
        "the failure must point at the file that decides the server kind:\n{combined}"
    );
}

#[test]
fn an_unknown_subcommand_is_rejected_with_a_nonzero_exit() {
    let home = TempDir::new("home");
    let elsewhere = TempDir::new("elsewhere");

    let output = Command::new(lambo())
        .arg("definitely-not-a-command")
        .current_dir(&elsewhere.path)
        .env(lambo_core::paths::HOME_ENV, &home.path)
        .env("NO_COLOR", "1")
        .output()
        .expect("lambo runs");

    assert!(
        !output.status.success(),
        "an unknown subcommand must not exit zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !stderr.is_empty(),
        "an unknown subcommand must say something"
    );
}

/// Exercise the HTTP helper in this integration-test executable, after a real
/// CLI invocation with captured output, rather than only in lambo-core's unit
/// test executable. Failure must retain the HTTP error, not become `None`.
#[test]
fn http_probe_after_captured_cli_output_preserves_response_and_error() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let output = Command::new(lambo()).arg("--version").output().unwrap();
    assert!(output.status.success());

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\nLambo PHP fixture OK",
            "not an HTTP response",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    let url = format!("http://localhost:{port}");
    let response = http_get(&url).expect("HTTP probe after Command::output");
    assert_eq!(response, (200, "Lambo PHP fixture OK".to_owned()));
    let error = http_get(&url).unwrap_err();
    assert!(matches!(error, lambo_core::error::Error::Http { .. }));
    assert!(error.to_string().contains(&url), "{error}");
    assert!(
        error.to_string().contains("without sending a response"),
        "{error}"
    );
    server.join().unwrap();
}

#[test]
fn bounded_cli_capture_preserves_success_and_failure_output() {
    let mut version = Command::new(lambo());
    version.arg("--version");
    let output = capture::output(&mut version, WAIT).expect("captured version");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("lambo"));

    let mut invalid = Command::new(lambo());
    invalid.arg("definitely-not-a-command");
    let output = capture::output(&mut invalid, WAIT).expect("captured CLI error");
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
}
