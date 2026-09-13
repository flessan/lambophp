//! A deterministic HTTP server used only by the lifecycle tests.
//!
//! This is **test infrastructure, not a product runtime**. It stands in for
//! PHP's built-in server so the real `up`/`down` orchestration can be exercised
//! on a machine with no PHP, Apache or MariaDB installed. It deliberately
//! accepts the same arguments `php -S` does:
//!
//! ```text
//! lambo-fixture-server -S 127.0.0.1:8080 -t /path/to/docroot
//!
//! ```
//!
//! That shape is the point. `php::serve_spec` builds exactly this command line,
//! so a test can install this binary as the `php` executable of a runtime and
//! then drive `session::up` unchanged - the spawn, the health check, the
//! service record and the shutdown are all the production code paths. Nothing
//! here reimplements the lifecycle, and production Apache support is untouched.
//!
//! It also answers the three probes Lambo makes of a PHP binary before it will
//! run one - `-v`, `--ini` and `-m` - in PHP's own output shapes. A stand-in
//! that modelled only `-S` was not a stand-in for PHP: Lambo starts a runtime to
//! confirm it works, and correctly refused this binary until it answered. The
//! parsers those probes feed are the same ones the real-artifact test checks
//! against a genuine PHP build.
//!
//! It is never part of the download catalogue and never ships in a release
//! package; see `docs/development.md`.
//!
//! # Failure modes
//!
//! A lifecycle test also has to prove what happens when a server *fails*.
//! Because the command line is fixed by `serve_spec`, failure is selected with
//! a marker file in the document root:
//!
//! - `fixture-exit` - print a diagnostic to stderr and exit non-zero without
//!   binding, which is what a server with a broken configuration does.
//! - `fixture-silent` - bind the port and accept connections, but never send a
//!   response. This is the case a listening socket cannot reveal and a real
//!   HTTP health check exists to catch.
//! - `fixture-slow` - stay alive but delay binding by the number of
//!   milliseconds in the file. Proves a slow start-up is waited for rather
//!   than mistaken for a dead process.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The body every successful response carries.
///
/// Fixed text, so a test can assert the bytes came from this server rather than
/// from anything else that might be listening.
const BODY: &str = "Lambo PHP fixture OK";

/// The version reported by `php -v` unless a control file says otherwise.
const DEFAULT_VERSION: &str = "8.4.2";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // This binary is installed as a runtime's `php`, so it has to answer what
    // Lambo asks of a PHP binary before it will run anything. A stand-in that
    // only modelled `-S` was not a stand-in for PHP: Lambo now starts a runtime
    // to confirm it works, and correctly refused this one.
    match args.first().map(String::as_str) {
        Some("-v") | Some("--version") => return version(),
        Some("--ini") => return ini(),
        Some("-m") => return modules(),
        _ => {}
    }

    let (Some(address), Some(docroot)) = (flag(&args, "-S"), flag(&args, "-t")) else {
        eprintln!("usage: lambo-fixture-server -S <host:port> -t <docroot>");
        return ExitCode::from(2);
    };
    let docroot = PathBuf::from(docroot);

    // A server with a broken configuration dies before it binds.
    if docroot.join("fixture-exit").exists() {
        eprintln!("fixture: refusing to start (fixture-exit marker present)");
        return ExitCode::from(3);
    }

    // A slow start-up: stay alive, but do not bind for a while. This is the
    // case a liveness check must *not* fail fast on - the process is healthy
    // and merely not ready yet, so the caller has to keep polling. The delay
    // is the marker file's contents in milliseconds, so a test can stay well
    // inside the caller's bound without hardcoding it.
    if let Ok(requested) = std::fs::read_to_string(docroot.join("fixture-slow")) {
        if let Ok(millis) = requested.trim().parse::<u64>() {
            println!("fixture: starting slowly, binding in {millis}ms");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_millis(millis));
        }
    }

    let listener = match TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(source) => {
            eprintln!("fixture: cannot bind {address}: {source}");
            return ExitCode::from(4);
        }
    };
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(address);

    // Written to stdout, which `ProcessSpec::log_to` redirects into the
    // service log, so a failure has something in it to point the user at.
    println!(
        "fixture: listening on {bound}, docroot {}",
        docroot.display()
    );
    let _ = std::io::stdout().flush();

    let silent = docroot.join("fixture-silent").exists();
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if silent {
            // Hold the connection open briefly, then drop it without writing.
            // The client sees a closed connection and no response.
            let _ = stream;
            continue;
        }
        if let Err(source) = serve(stream, &docroot) {
            eprintln!("fixture: {source}");
        }
    }
    ExitCode::SUCCESS
}

/// Answers one request.
fn serve(mut stream: TcpStream, docroot: &Path) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Read the request headers. The health check sends `Connection: close`, so
    // one bounded read is enough; a keep-alive client is not something this
    // fixture needs to support.
    let mut buffer = [0u8; 4096];
    let _ = stream.read(&mut buffer)?;

    let body = BODY.as_bytes();
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         X-Lambo-Docroot: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len(),
        header_safe(&docroot.display().to_string())
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Reads the value following `name` in an argument list.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// Strips characters that cannot appear in an HTTP header value.
fn header_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii() && !c.is_control() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The PHP probes
//
// Installed as a runtime's `php`, this binary answers the three questions Lambo
// asks before it will run a runtime. The output shapes are PHP's, not invented
// ones: `RuntimeHealth` parses them with the same parser the real-artifact test
// checks against a genuine build, so a drift here shows up there too.
// ---------------------------------------------------------------------------

/// The configuration file PHP would load, resolved from `PHPRC`.
///
/// PHP accepts either a directory containing `php.ini` or the path to the file
/// itself, and so does this.
fn loaded_ini() -> Option<PathBuf> {
    let phprc = std::env::var_os("PHPRC")?;
    let path = PathBuf::from(phprc);
    if path.is_dir() {
        let candidate = path.join("php.ini");
        return candidate.is_file().then_some(candidate);
    }
    path.is_file().then_some(path)
}

/// The version to report: a control file beside the binary, else the default.
///
/// Same convention as the shell fixture in `tests/fixtures/runtime/`, so a test
/// can make one specific installed runtime misbehave without touching the
/// process environment.
fn reported_version() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|dir| std::fs::read_to_string(dir.join(".fixture-version")).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| DEFAULT_VERSION.to_owned())
}

fn version() -> ExitCode {
    let version = reported_version();
    println!("PHP {version} (cli) (built: Dec 17 2024 18:22:53) (NTS)");
    println!("Copyright (c) The PHP Group");
    println!("Zend Engine v4.4.2, Copyright (c) Zend Technologies");
    ExitCode::SUCCESS
}

fn ini() -> ExitCode {
    println!("Configuration File (php.ini) Path: /lambo/fixture/etc");
    match loaded_ini() {
        Some(path) => println!("Loaded Configuration File:         {}", path.display()),
        None => println!("Loaded Configuration File:         (none)"),
    }
    println!("Scan this dir for additional .ini files: (none)");
    println!("Additional .ini files parsed:      (none)");
    ExitCode::SUCCESS
}

fn modules() -> ExitCode {
    println!("[PHP Modules]");
    println!("Core");
    println!("date");
    println!("standard");
    // Derived from the configuration Lambo generated, so a test asserting the
    // module list is really asserting that PHPRC reached this process.
    if let Some(path) = loaded_ini() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Some(name) = line.strip_prefix("extension=") {
                    println!("{}", name.trim());
                }
            }
        }
    }
    println!();
    println!("[Zend Modules]");
    ExitCode::SUCCESS
}
