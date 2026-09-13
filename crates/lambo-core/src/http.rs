//! A very small HTTP/1.1 client, used for health checks.
//!
//! `lambo up` has to answer one question: *is the server actually serving?*
//! A listening socket is not enough - Apache can bind port 8080 and still be
//! broken (bad `php.ini`, missing module, wrong document root). Only a real
//! request tells the truth, and Lambo must never report a service as running
//! when it is not.
//!
//! Rather than pull in an async HTTP stack for a single `GET`, this module
//! implements the handful of HTTP/1.1 that a health check needs: one request,
//! `Connection: close`, status line, a few headers, bounded body, redirect
//! following. It is synchronous, dependency-free, and behaves the same on
//! Windows and Unix because it only uses [`std::net::TcpStream`].
//!
//! TLS is deliberately out of scope: health checks run against `localhost`
//! over plain HTTP, which is also what the default (unprivileged, non-admin)
//! Lambo workflow uses.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::{Error, Result};

/// Largest response body that is read (1 MiB).
///
/// A health check never needs a page; the cap keeps a misconfigured server
/// from making `lambo up` hang or balloon memory.
const MAX_BODY: usize = 1024 * 1024;

/// How many redirects are followed.
const MAX_REDIRECTS: usize = 5;

/// Default timeout for a health check.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Status code, e.g. `200`.
    pub status: u16,
    /// Response headers, lower-cased keys, in the order received.
    pub headers: Vec<(String, String)>,
    /// Body, truncated at [`MAX_BODY`].
    pub body: String,
}

impl Response {
    /// Whether the status indicates the server is serving normally.
    ///
    /// 2xx and 3xx are healthy; a 404 from Apache means the server works but
    /// the document root is wrong, which `lambo doctor` reports separately.
    pub fn is_healthy(&self) -> bool {
        (200..400).contains(&self.status)
    }

    /// Value of a header, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.as_str())
    }
}

/// A parsed `http://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Host name or IP literal.
    pub host: String,
    /// Port (80 when the URL omits it).
    pub port: u16,
    /// Path including the leading `/`.
    pub path: String,
}

impl Url {
    /// The `http://host[:port]` prefix.
    pub fn origin(&self) -> String {
        if self.port == 80 {
            format!("http://{}", self.host)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

/// Parses an `http://` URL.
///
/// Only the `http` scheme is accepted: this client has no TLS support and
/// refuses to pretend otherwise.
pub fn parse_url(input: &str) -> Result<Url> {
    let rest = input
        .trim()
        .strip_prefix("http://")
        .ok_or_else(|| invalid_url(input, "the URL must start with http://"))?;

    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    if authority.is_empty() {
        return Err(invalid_url(input, "the URL has no host"));
    }

    // An IPv6 literal arrives bracketed: http://[::1]:8080/
    let (host, port_text) = match authority.split_once(']') {
        Some((host, remainder)) => {
            let host = host.trim_start_matches('[').to_owned();
            (host, remainder.strip_prefix(':').map(str::to_owned))
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port.to_owned())),
            None => (authority.to_owned(), None),
        },
    };

    let port = match port_text {
        Some(text) => text
            .parse::<u16>()
            .map_err(|_| invalid_url(input, &format!("`{text}` is not a valid port")))?,
        None => 80,
    };

    if host.is_empty() {
        return Err(invalid_url(input, "the URL has no host"));
    }
    Ok(Url { host, port, path })
}

/// Performs a `GET` request, following redirects.
pub fn get(url: &str, timeout: Duration) -> Result<Response> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let parsed = parse_url(&current)?;
        let response = request(&parsed, timeout)?;
        if response.status < 300 || response.status > 399 {
            return Ok(response);
        }
        let Some(location) = response.header("location") else {
            return Ok(response);
        };
        current = resolve_location(&parsed, location);
    }
    Err(Error::Http {
        url: url.to_owned(),
        reason: format!("more than {MAX_REDIRECTS} redirects"),
    })
}

/// Performs one request without following redirects.
fn request(url: &Url, timeout: Duration) -> Result<Response> {
    let address = format!("{}:{}", url.host, url.port);
    let candidates = address
        .to_socket_addrs()
        .map_err(|source| http_error(url, &format!("cannot resolve `{}`: {source}", url.host)))?
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(http_error(url, &format!("cannot resolve `{}`", url.host)));
    }

    // Every resolved address is tried, not just the first. `localhost`
    // commonly resolves to `::1` before `127.0.0.1`, while the servers Lambo
    // starts bind the IPv4 loopback explicitly. Taking only the first result
    // made the health check refuse a connection to a server that was serving
    // perfectly, and `lambo up` then reported "the server started but never
    // answered an HTTP request" - a false negative against a healthy service.
    let mut last = String::new();
    let mut connected = None;
    for candidate in &candidates {
        match TcpStream::connect_timeout(candidate, timeout) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(source) => last = source.to_string(),
        }
    }
    let mut stream =
        connected.ok_or_else(|| http_error(url, &format!("connection refused ({last})")))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|source| http_error(url, &source.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|source| http_error(url, &source.to_string()))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: lambo-php\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        if url.port == 80 {
            url.host.clone()
        } else {
            format!("{}:{}", url.host, url.port)
        }
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|source| http_error(url, &source.to_string()))?;
    stream
        .flush()
        .map_err(|source| http_error(url, &source.to_string()))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&chunk[..read]);
                if raw.len() > MAX_BODY * 2 {
                    break;
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(source) if source.kind() == std::io::ErrorKind::TimedOut => break,
            Err(source) if raw.is_empty() => return Err(http_error(url, &source.to_string())),
            Err(_) => break,
        }
    }

    parse_response(url, &raw)
}

/// Splits a raw response into status, headers and body.
fn parse_response(url: &Url, raw: &[u8]) -> Result<Response> {
    let split = find_header_end(raw).ok_or_else(|| {
        http_error(
            url,
            "the server closed the connection without sending a response",
        )
    })?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body_bytes = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| http_error(url, &format!("unparsable status line `{status_line}`")))?;

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();

    let body = String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(MAX_BODY)]).into_owned();
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Finds the `\r\n\r\n` separator between headers and body.
fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Resolves a `Location` header against the URL it came from.
pub fn resolve_location(from: &Url, location: &str) -> String {
    let location = location.trim();
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.to_owned();
    }
    if let Some(rest) = location.strip_prefix('/') {
        return format!("{}/{}", from.origin(), rest);
    }
    let base = match from.path.rsplit_once('/') {
        Some((prefix, _)) => prefix.to_owned(),
        None => String::new(),
    };
    format!("{}{base}/{location}", from.origin())
}

/// Whether a URL answers with a healthy status.
pub fn is_up(url: &str) -> bool {
    get(url, DEFAULT_TIMEOUT)
        .map(|response| response.is_healthy())
        .unwrap_or(false)
}

/// Polls `url` until it answers or the timeout elapses.
///
/// This is how `lambo up` waits for Apache: a service that binds its port but
/// cannot serve yet would otherwise be reported as running.
pub fn wait_until_up(url: &str, timeout: Duration) -> Result<Response> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = "no response".to_owned();
    while std::time::Instant::now() < deadline {
        match get(url, DEFAULT_TIMEOUT.min(timeout)) {
            Ok(response) if response.is_healthy() => return Ok(response),
            Ok(response) => last = format!("HTTP {}", response.status),
            Err(error) => last = error.to_string(),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(Error::Timeout {
        service: format!("{url} (last error: {last})"),
        seconds: timeout.as_secs(),
    })
}

/// Builds an [`Error::Http`].
fn http_error(url: &Url, reason: &str) -> Error {
    Error::Http {
        url: format!("{}{}", url.origin(), url.path),
        reason: reason.to_owned(),
    }
}

/// Builds an [`Error::InvalidInput`] for a malformed URL.
fn invalid_url(input: &str, reason: &str) -> Error {
    Error::InvalidInput(format!("invalid URL `{input}`: {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Starts a one-shot server that replies with `response` to one request.
    /// Returns the base URL and a handle to the thread.
    fn serve_once(response: &'static str) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).unwrap_or(0);
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            buffer[..read].to_vec()
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    #[test]
    fn a_hostname_that_resolves_to_an_unreachable_address_first_still_connects() {
        // `localhost` commonly resolves to `::1` before `127.0.0.1`, while the
        // servers Lambo starts bind the IPv4 loopback explicitly. Taking only
        // the first resolved address therefore refused a connection to a
        // server that was serving fine, and `lambo up` reported "the server
        // started but never answered an HTTP request".
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let body = "served over the loopback that resolved second";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        // Whether this host resolves localhost to ::1 first is outside the
        // test's control, so the assertion is that a name resolving to
        // *several* addresses reaches the one that is listening.
        let response = get(&format!("http://localhost:{port}"), Duration::from_secs(5))
            .expect("a hostname with several addresses must still reach a listening server");
        assert_eq!(response.status, 200);
        assert!(response.body.starts_with("served over the loopback"));
        handle.join().unwrap();
    }

    #[test]
    fn parses_urls_with_and_without_ports_and_paths() {
        assert_eq!(
            parse_url("http://localhost:8080").unwrap(),
            Url {
                host: "localhost".to_owned(),
                port: 8080,
                path: "/".to_owned()
            }
        );
        assert_eq!(
            parse_url("http://localhost").unwrap(),
            Url {
                host: "localhost".to_owned(),
                port: 80,
                path: "/".to_owned()
            }
        );
        assert_eq!(
            parse_url("http://127.0.0.1:8081/index.php?page=1").unwrap(),
            Url {
                host: "127.0.0.1".to_owned(),
                port: 8081,
                path: "/index.php?page=1".to_owned(),
            }
        );
        assert_eq!(parse_url("http://[::1]:8080/x").unwrap().host, "::1");
        assert_eq!(
            parse_url("http://localhost:8080").unwrap().origin(),
            "http://localhost:8080"
        );
        assert_eq!(
            parse_url("http://example.com/a").unwrap().origin(),
            "http://example.com"
        );
    }

    #[test]
    fn rejects_urls_it_cannot_serve() {
        for bad in [
            "https://localhost",
            "localhost:8080",
            "http://",
            "http://:8080/",
            "http://localhost:notaport",
        ] {
            assert!(parse_url(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn reads_a_real_response() {
        let (url, handle) = serve_once(
            "HTTP/1.1 200 OK\r\nServer: Apache/2.4.62\r\nContent-Type: text/html\r\n\r\n<html>hi</html>",
        );

        let response = get(&url, Duration::from_secs(5)).unwrap();
        assert_eq!(response.status, 200);
        assert!(response.is_healthy());
        assert_eq!(response.header("server"), Some("Apache/2.4.62"));
        assert_eq!(response.body, "<html>hi</html>");

        let request = handle.join().unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("GET / HTTP/1.1\r\n"));
        assert!(
            text.contains("Connection: close"),
            "the client must not keep the socket open"
        );
    }

    #[test]
    fn reports_unhealthy_statuses() {
        let (url, handle) = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let response = get(&url, Duration::from_secs(5)).unwrap();
        assert_eq!(response.status, 404);
        assert!(!response.is_healthy());
        handle.join().unwrap();
    }

    #[test]
    fn follows_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                let response = if index == 0 {
                    "HTTP/1.1 302 Found\r\nLocation: /login\r\nContent-Length: 0\r\n\r\n".to_owned()
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_owned()
                };
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        let response = get(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(5)).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
        server.join().unwrap();
    }

    #[test]
    fn connection_refused_is_an_error_not_a_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let error = get(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(2)).unwrap_err();
        assert!(matches!(error, Error::Http { .. }));
        assert!(error.to_string().contains("connection refused"), "{error}");
        assert!(!is_up(&format!("http://127.0.0.1:{port}/")));
    }

    #[test]
    fn a_silent_server_times_out_with_an_explanation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Hold the connection open without answering.
            thread::sleep(Duration::from_secs(2));
            drop(stream);
        });

        let error = get(
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("closed the connection")
                || error.to_string().contains("timed out"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn wait_until_up_retries_until_the_server_serves() {
        // A port that is bound but not yet serving is exactly the state
        // Apache is in for a moment after `lambo up` starts it: the socket
        // accepts, nothing answers. The first connection is dropped without a
        // response, the second gets a real answer.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/");

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            stream.flush().unwrap();
        });

        let response = wait_until_up(&url, Duration::from_secs(10)).unwrap();
        assert_eq!(response.status, 200);
        server.join().unwrap();
    }

    #[test]
    fn wait_until_up_gives_up_with_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let error = wait_until_up(
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_millis(600),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
        assert!(error.to_string().contains("connection refused"), "{error}");
    }

    #[test]
    fn locations_are_resolved_against_their_url() {
        let from = Url {
            host: "localhost".to_owned(),
            port: 8080,
            path: "/app/index.php".to_owned(),
        };
        assert_eq!(
            resolve_location(&from, "/login"),
            "http://localhost:8080/login"
        );
        assert_eq!(
            resolve_location(&from, "assets/app.css"),
            "http://localhost:8080/app/assets/app.css"
        );
        assert_eq!(
            resolve_location(&from, "http://other.test/x"),
            "http://other.test/x"
        );
    }
}
