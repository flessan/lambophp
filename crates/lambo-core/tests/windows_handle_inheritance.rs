//! Real three-process regression: test -> captured launcher -> live service.
//! The control socket only coordinates lifetime; it is not an HTTP health check.
//! The service exits cooperatively AFTER capture completes, never to force EOF.
//!
//! Two spawn shapes are covered, because the engine uses both for services:
//! [`detached_service_does_not_hold_captured_launcher_pipes`] for the detached,
//! log-file shape and [`piped_service_does_not_hold_captured_launcher_pipes`]
//! for the supervised shape whose output the launcher streams through pipes.
#![cfg(windows)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use lambo_core::platform::Os;
use lambo_core::process::{self, ProcessSpec};

const WAIT: Duration = Duration::from_secs(30);
const ROLE: &str = "LAMBO_HANDLE_REGRESSION_ROLE";
const ARGUMENT: &str = "unmatched \"quoted\" filter λ\\";

#[test]
fn detached_service_does_not_hold_captured_launcher_pipes() {
    let root = std::env::temp_dir().join(format!(
        "lambo handle regression {} {}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut parent = Command::new(std::env::current_exe().unwrap());
    parent
        .args(["--exact", "launcher_or_service", "--nocapture"])
        .env(ROLE, "launcher")
        .env("LAMBO_HANDLE_ADDRESS", &address)
        .env("LAMBO_HANDLE_ROOT", &root);

    let (captured_tx, captured_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = captured_tx.send(parent.output());
    });
    let (connected_tx, connected_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = connected_tx.send(listener.accept());
    });
    let (mut service, _) = connected_rx
        .recv_timeout(WAIT)
        .expect("detached service must connect")
        .unwrap();
    service.set_read_timeout(Some(WAIT)).unwrap();
    service.set_write_timeout(Some(WAIT)).unwrap();
    let mut pid = [0; 4];
    service.read_exact(&mut pid).expect("service ready");
    let pid = u32::from_le_bytes(pid);

    // On the old Windows spawn path this times out: the launcher has exited,
    // but the detached service still owns unrelated copies of both pipe writers.
    let captured = captured_rx.recv_timeout(WAIT);
    let alive_after_parent = process::is_running(pid, Os::host());

    // Always release the service before asserting (including on old-code failure).
    // Receiving EOF above must NOT depend on this release. No process is killed.
    service.write_all(b"x").unwrap();
    let mut finished = Vec::new();
    service
        .read_to_end(&mut finished)
        .expect("service exits cooperatively");

    let output = captured
        .expect("launcher output must reach EOF while its service is still alive")
        .expect("capture launcher output");
    assert!(output.status.success(), "{output:?}");
    assert!(
        alive_after_parent,
        "service must survive the launcher exiting"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("launcher stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("launcher stderr"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("service stdout"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("service stderr"));
    let log = fs::read_to_string(root.join("logs/service.log")).unwrap();
    assert!(log.contains("service stdout"), "{log}");
    assert!(log.contains("service stderr"), "{log}");
    assert_eq!(finished, b"finished");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn piped_service_does_not_hold_captured_launcher_pipes() {
    let root = std::env::temp_dir().join(format!(
        "lambo piped handle regression {} {}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut parent = Command::new(std::env::current_exe().unwrap());
    parent
        .args(["--exact", "launcher_or_service", "--nocapture"])
        .env(ROLE, "piped-launcher")
        .env("LAMBO_HANDLE_ADDRESS", &address)
        .env("LAMBO_HANDLE_ROOT", &root);

    let (captured_tx, captured_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = captured_tx.send(parent.output());
    });
    let (connected_tx, connected_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = connected_tx.send(listener.accept());
    });
    let mut service = match connected_rx.recv_timeout(WAIT) {
        Ok(Ok((service, _))) => service,
        other => {
            // The service never connected. The launcher's captured output says
            // why: a failed spawn panics there, and with no service holding
            // its pipes the capture completes promptly.
            let output = captured_rx
                .recv_timeout(WAIT)
                .expect("the service never connected and the launcher never exited")
                .expect("capture launcher output");
            panic!(
                "the piped service never connected (accept: {other:?}); the launcher said:\n\
                 --- stdout ---\n{}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    };
    service.set_read_timeout(Some(WAIT)).unwrap();
    service.set_write_timeout(Some(WAIT)).unwrap();
    let mut pid = [0; 4];
    service.read_exact(&mut pid).expect("service ready");
    let pid = u32::from_le_bytes(pid);

    // The launcher has long exited by now: it starts the service, scans the
    // service's pipes for the handoff markers, prints its report, and returns.
    // On a spawn path that leaks the launcher's captured stdio into the
    // service this times out, because the service keeps the capture pipes
    // open until it stops.
    let captured = captured_rx.recv_timeout(WAIT);
    let alive_after_parent = process::is_running(pid, Os::host());

    // Always release the service before asserting (including on old-code failure).
    // Receiving EOF above must NOT depend on this release. No process is killed.
    service.write_all(b"x").unwrap();
    let mut finished = Vec::new();
    service
        .read_to_end(&mut finished)
        .expect("service exits cooperatively");

    let output = captured
        .expect("launcher output must reach EOF while its service is still alive")
        .expect("capture launcher output");
    assert!(output.status.success(), "{output:?}");
    assert!(
        alive_after_parent,
        "service must survive the launcher exiting"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("launcher stdout: service"), "{stdout}");
    assert!(
        stdout.contains("piped service stdout"),
        "the launcher must have read the service's piped stdout: {stdout}"
    );
    assert!(
        stdout.contains("piped service stderr"),
        "the launcher must have read the service's piped stderr: {stdout}"
    );
    assert_eq!(finished, b"finished");
    let _ = fs::remove_dir_all(root);
}

// A subprocess entry point in this very test executable. Without the private
// role variable it is a no-op, so ordinary cargo test needs no special flags.
#[test]
fn launcher_or_service() {
    let role = match std::env::var(ROLE) {
        Ok(role) => role,
        Err(_) => return,
    };
    let root = std::path::PathBuf::from(std::env::var_os("LAMBO_HANDLE_ROOT").unwrap());
    match role.as_str() {
        "launcher" => {
            let spec = ProcessSpec::new(std::env::current_exe().unwrap(), "handle regression")
                .args([
                    "--exact",
                    "launcher_or_service",
                    "--nocapture",
                    "--skip",
                    ARGUMENT,
                ])
                .cwd(&root)
                .env(ROLE, "service")
                .env(
                    "LAMBO_HANDLE_VALUE",
                    "quoted \"value\" with spaces and Unicode λ",
                )
                .log_to(root.join("logs/service.log"))
                .detached();
            let child = process::spawn(&spec, Os::host()).expect("spawn detached service");
            println!("launcher stdout: service {}", child.id());
            eprintln!("launcher stderr");
            drop(child); // Must not terminate the service.
        }
        "service" => {
            assert!(std::env::args().any(|arg| arg == ARGUMENT));
            assert_eq!(
                std::env::var("LAMBO_HANDLE_VALUE").unwrap(),
                "quoted \"value\" with spaces and Unicode λ",
            );
            assert_eq!(
                fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
                fs::canonicalize(root).unwrap(),
            );
            assert_eq!(
                std::io::stdin().read(&mut [0]).unwrap(),
                0,
                "service stdin is NUL"
            );
            println!("service stdout");
            eprintln!("service stderr");
            std::io::stdout().flush().unwrap();
            std::io::stderr().flush().unwrap();
            let mut control =
                TcpStream::connect(std::env::var("LAMBO_HANDLE_ADDRESS").unwrap()).unwrap();
            control.set_read_timeout(Some(WAIT)).unwrap();
            control.set_write_timeout(Some(WAIT)).unwrap();
            control
                .write_all(&std::process::id().to_le_bytes())
                .unwrap();
            let mut release = [0];
            control
                .read_exact(&mut release)
                .expect("parent test releases service");
            assert_eq!(release, *b"x");
            control.write_all(b"finished").unwrap();
        }
        "piped-launcher" => {
            let spec = ProcessSpec::new(std::env::current_exe().unwrap(), "handle regression")
                .args([
                    "--exact",
                    "launcher_or_service",
                    "--nocapture",
                    "--skip",
                    ARGUMENT,
                ])
                .cwd(&root)
                .env(ROLE, "piped-service")
                .env(
                    "LAMBO_HANDLE_ADDRESS",
                    std::env::var("LAMBO_HANDLE_ADDRESS").unwrap(),
                );
            let process::ServiceChild {
                mut child,
                pid,
                stdout,
                stderr,
            } = process::spawn_service(&spec, Os::host()).expect("spawn piped service");
            {
                let mut console = std::io::stdout().lock();
                writeln!(console, "launcher stdout: service {pid}").unwrap();
                console.flush().unwrap();
            }
            // The test harness prints its own banner to the service's stdout
            // before the role runs, so scan both pipes line by line for the
            // role's handoff markers. If the service dies early both pipes
            // EOF, the scan ends, and the report below carries the exit code
            // and everything the service managed to print - that is what
            // diagnoses a service that never reaches the control socket.
            let mut stdout = BufReader::new(stdout);
            let mut stderr = BufReader::new(stderr);
            let mut seen_stdout = String::new();
            let mut seen_stderr = String::new();
            let mut stdout_done = false;
            let mut stderr_done = false;
            loop {
                if seen_stdout.contains("piped service stdout") {
                    stdout_done = true;
                }
                if seen_stderr.contains("piped service stderr") {
                    stderr_done = true;
                }
                if stdout_done && stderr_done {
                    break;
                }
                if !stdout_done {
                    let mut line = String::new();
                    let read = stdout.read_line(&mut line).expect("read piped stdout");
                    if read == 0 {
                        stdout_done = true;
                    } else {
                        seen_stdout.push_str(&line);
                    }
                    continue;
                }
                let mut line = String::new();
                let read = stderr.read_line(&mut line).expect("read piped stderr");
                if read == 0 {
                    stderr_done = true;
                } else {
                    seen_stderr.push_str(&line);
                }
            }
            let outcome = match child.try_wait() {
                Ok(Some(status)) => format!(
                    "EXITED before connecting: {status:?}, code {:?}",
                    status.code(),
                ),
                Ok(None) => "still alive at handoff".to_owned(),
                Err(error) => format!("try_wait failed: {error}"),
            };
            let mut report = format!(
                "launcher report: service {pid} wrote {} stdout bytes and {} stderr bytes\n\
                 launcher report: service {pid} {outcome}\n",
                seen_stdout.len(),
                seen_stderr.len(),
            );
            report.push_str("--- service stdout ---\n");
            report.push_str(&seen_stdout);
            report.push_str("--- service stderr ---\n");
            report.push_str(&seen_stderr);
            report.push_str("--- end of service output ---\n");
            let mut console = std::io::stdout().lock();
            write!(console, "{report}").unwrap();
            console.flush().unwrap();
            drop(child); // Must not terminate the service.
        }
        "piped-service" => {
            assert!(std::env::args().any(|arg| arg == ARGUMENT));
            assert_eq!(
                fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
                fs::canonicalize(root).unwrap(),
            );
            println!("piped service stdout");
            eprintln!("piped service stderr");
            std::io::stdout().flush().unwrap();
            std::io::stderr().flush().unwrap();
            let mut control =
                TcpStream::connect(std::env::var("LAMBO_HANDLE_ADDRESS").unwrap()).unwrap();
            control.set_read_timeout(Some(WAIT)).unwrap();
            control.set_write_timeout(Some(WAIT)).unwrap();
            control
                .write_all(&std::process::id().to_le_bytes())
                .unwrap();
            let mut release = [0];
            control
                .read_exact(&mut release)
                .expect("parent test releases service");
            assert_eq!(release, *b"x");
            control.write_all(b"finished").unwrap();
            // Silent from here on: the launcher that owned the read ends of
            // the stdout and stderr pipes has already exited, and a write to
            // them would fail with a broken pipe.
        }
        other => panic!("unknown role {other}"),
    }
}
