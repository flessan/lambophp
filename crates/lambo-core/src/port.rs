//! TCP port availability and conflict diagnosis.
//!
//! Every service Lambo manages owns a port, and on a developer machine those
//! ports are frequently taken: a second MySQL, a Docker container, or the
//! previous `lambo up` that did not shut down. Detecting that *before*
//! starting anything - and naming the process that holds the port - is the
//! difference between a useful error message and a log file full of noise.
//!
//! Two properties matter:
//!
//! - **Never kill anything.** This module only observes. Offering to stop a
//!   conflicting process is a decision for the user (see `lambo doctor`).
//! - **Work without privileges.** Reading `netstat`, `lsof` or `/proc` needs
//!   no administrator rights on any platform.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::platform::Os;

/// How long a probe waits before giving up.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Whether nothing is currently listening on `port` (loopback).
///
/// Binds instead of connecting: a port that is free to bind is free to use,
/// and a service bound to `0.0.0.0` correctly makes the bind fail.
pub fn is_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Whether something answers on `port` right now.
pub fn is_listening(port: u16) -> bool {
    TcpStream::connect_timeout(&loopback_addr(port), PROBE_TIMEOUT).is_ok()
}

/// The loopback address Lambo's services bind to.
pub fn loopback_addr(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], port))
}

/// Verifies that `port` can be used, naming the occupant when it cannot.
pub fn check(port: u16, os: Os) -> Result<()> {
    if is_free(port) {
        return Ok(());
    }
    Err(Error::PortInUse {
        port,
        occupied_by: occupant(port, os),
    })
}

/// Why a loopback bind failed, or that it did not.
///
/// Distinguishing these matters because they have different remedies. "Another
/// process holds it" means stop that process or move. "The OS refused" on a
/// port below 1024 means the Unix privileged-port rule, and the answer is to
/// move - not to stop anything, and not to demand root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    /// Nothing is listening; the bind succeeded.
    Free,
    /// Another process holds the port.
    Occupied,
    /// The operating system refused the bind.
    ///
    /// On Unix this is the privileged-port rule for ports below 1024. On
    /// Windows it is a reserved or excluded port range.
    Refused,
    /// Some other error (no loopback interface, and similar).
    Other,
}

/// Attempts a loopback bind and reports what happened.
///
/// Observing the real error is the point: guessing "port 80 must be a
/// permissions problem on Unix" would be wrong on Windows, and wrong on Unix
/// too whenever something is genuinely listening there.
pub fn bind_outcome(port: u16) -> BindOutcome {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => BindOutcome::Free,
        Err(error) => match error.kind() {
            std::io::ErrorKind::AddrInUse => BindOutcome::Occupied,
            std::io::ErrorKind::PermissionDenied => BindOutcome::Refused,
            _ => BindOutcome::Other,
        },
    }
}

/// The port a server will actually listen on, and why it is not the requested
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPort {
    /// The port to bind.
    pub port: u16,
    /// The port that was asked for.
    pub requested: u16,
    /// Why the two differ, or `None` when they do not.
    pub reason: Option<PortFallback>,
}

impl ResolvedPort {
    /// Whether the requested port was used unchanged.
    pub fn is_exact(&self) -> bool {
        self.port == self.requested
    }

    /// One line explaining the change, for the user.
    ///
    /// `None` when nothing changed - callers then stay quiet, because
    /// narrating a non-event is noise.
    pub fn explanation(&self, os: Os) -> Option<String> {
        let reason = self.reason.as_ref()?;
        let url = crate::naming::local_url(self.port);
        let wanted = crate::naming::local_url(self.requested);
        Some(match reason {
            PortFallback::Refused if os.is_windows() => format!(
                "port {} is reserved or excluded on this machine, so {} is served on {url} instead",
                self.requested, wanted
            ),
            PortFallback::Refused => format!(
                "binding port {} needs root on {}, so {} is served on {url} instead - no \
                 administrator rights are needed at this port",
                self.requested,
                os.display_name(),
                wanted
            ),
            PortFallback::Occupied { .. } => format!(
                "port {} is already in use{}, so {} is served on {url} instead",
                self.requested,
                occupant_hint(self.occupant()),
                wanted
            ),
            PortFallback::Unavailable => format!(
                "port {} could not be bound, so {} is served on {url} instead",
                self.requested, wanted
            ),
        })
    }

    /// The process holding the port, when the fallback was a conflict.
    pub fn occupant(&self) -> Option<&str> {
        match &self.reason {
            Some(PortFallback::Occupied { occupant }) => occupant.as_deref(),
            _ => None,
        }
    }
}

/// Why the requested port could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortFallback {
    /// The OS refused the bind: a privileged port on Unix, a reserved range on
    /// Windows.
    Refused,
    /// Another process is listening.
    Occupied {
        /// The process name, when the platform can identify it.
        occupant: Option<String>,
    },
    /// The bind failed for some other reason.
    Unavailable,
}

/// Decides which port a server should listen on.
///
/// Lambo defaults to port 80 so the URL is `http://localhost`, the way the
/// product reads. That port is not always available, and the two reasons it
/// is not have different meanings:
///
/// - On **Windows** a standard user can bind 80, so this normally succeeds.
///   When it does not, something else holds the port or the range is excluded.
/// - On **Linux and macOS** ports below 1024 need root or
///   `CAP_NET_BIND_SERVICE`. Demanding that would break the first run on a
///   locked-down machine, so Lambo moves to 8080 and says so.
///
/// Either way the caller gets the real port and a ready-made explanation. Ports
/// are an implementation detail to the user; the URL is not, so the change is
/// always reported rather than absorbed silently.
pub fn resolve_listen_port(requested: u16, os: Os) -> ResolvedPort {
    let exact = ResolvedPort {
        port: requested,
        requested,
        reason: None,
    };

    match bind_outcome(requested) {
        BindOutcome::Free => exact,
        BindOutcome::Refused => fallback(
            requested,
            PortFallback::Refused,
            // Nothing to look up: the OS refused before anything listened.
            None,
            os,
        ),
        BindOutcome::Occupied => {
            let occupant = occupant(requested, os);
            fallback(
                requested,
                PortFallback::Occupied {
                    occupant: occupant.clone(),
                },
                occupant,
                os,
            )
        }
        BindOutcome::Other => fallback(requested, PortFallback::Unavailable, None, os),
    }
}

/// Picks the first candidate that binds, keeping the requested port's reason.
fn fallback(
    requested: u16,
    reason: PortFallback,
    occupant: Option<String>,
    _os: Os,
) -> ResolvedPort {
    for candidate in fallback_candidates(requested) {
        if bind_outcome(candidate) == BindOutcome::Free {
            return ResolvedPort {
                port: candidate,
                requested,
                reason: Some(match &reason {
                    PortFallback::Occupied { .. } => PortFallback::Occupied {
                        occupant: occupant.clone(),
                    },
                    other => other.clone(),
                }),
            };
        }
    }
    // Every candidate is taken too. Report the original problem: inventing a
    // port would produce a URL Lambo cannot actually serve.
    ResolvedPort {
        port: requested,
        requested,
        reason: Some(reason),
    }
}

/// Ports to fall back to when the requested one is unavailable.
///
/// 80 → 8080 and 443 → 8443 keep the mapping users already know from other
/// local stacks; anything else steps upward from its own value.
fn fallback_candidates(requested: u16) -> Vec<u16> {
    match requested {
        80 => vec![8080, 8081, 8082],
        443 => vec![8443, 8444, 8445],
        other => alternatives(other),
    }
}

/// The " (`process`)" suffix used in port-conflict messages.
fn occupant_hint(occupant: Option<&str>) -> String {
    match occupant {
        Some(name) => format!(" by `{name}`"),
        None => String::new(),
    }
}

/// Finds the first free port among `candidates`.
pub fn first_free(candidates: impl IntoIterator<Item = u16>) -> Option<u16> {
    candidates.into_iter().find(|port| is_free(*port))
}

/// Ports to suggest when `port` is taken.
///
/// The suggestions stay in the unprivileged range (above 1024) so accepting
/// one never requires administrator rights.
pub fn alternatives(port: u16) -> Vec<u16> {
    let mut suggestions = Vec::with_capacity(3);
    let mut candidate = port.saturating_add(1).max(1025);
    loop {
        suggestions.push(candidate);
        if suggestions.len() == 3 || candidate == u16::MAX {
            break;
        }
        candidate = candidate.saturating_add(1);
    }
    suggestions
}

/// Names the process listening on `port`, when the platform can tell us.
///
/// Best-effort by design: `None` means "unknown", and callers degrade to a
/// plain "port in use" message rather than failing.
pub fn occupant(port: u16, os: Os) -> Option<String> {
    let pid = match os {
        Os::Windows => netstat_pid(port),
        Os::MacOs => lsof_pid(port),
        Os::Linux => proc_net_pid(port).or_else(|| lsof_pid(port)),
        Os::OtherUnix => lsof_pid(port),
    }?;

    let name = process_name(pid, os)?;
    Some(format!("{name} (PID {pid})"))
}

/// Asks `netstat` for the PID listening on `port` (Windows).
fn netstat_pid(port: u16) -> Option<u32> {
    let output = Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    netstat_listener_pid(&String::from_utf8_lossy(&output.stdout), port)
}

/// Asks `lsof` for the PID listening on `port` (macOS, and Linux fallback).
fn lsof_pid(port: u16) -> Option<u32> {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    lsof_listener_pid(&String::from_utf8_lossy(&output.stdout), port)
}

/// Reads the listening PID for `port` from Linux's `/proc/net/tcp`.
fn proc_net_pid(port: u16) -> Option<u32> {
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find_map(|content| proc_net_tcp_inode(&content, port))?;
    proc_pid_for_inode(inode)
}

/// Scans `/proc/<pid>/fd` for the socket inode, returning its owner.
///
/// Only processes owned by the current user are readable, which is exactly
/// the interesting case: a port held by another user's process is reported as
/// "in use" without a name rather than causing a permission error.
fn proc_pid_for_inode(inode: u64) -> Option<u32> {
    let needle = format!("socket:[{inode}]");
    let proc = PathBuf::from("/proc");
    for entry in std::fs::read_dir(&proc).ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|text| text.parse::<u32>().ok()) else {
            continue;
        };
        let fds = proc.join(pid.to_string()).join("fd");
        let Ok(handles) = std::fs::read_dir(&fds) else {
            continue;
        };
        for handle in handles.flatten() {
            if std::fs::read_link(handle.path())
                .ok()
                .is_some_and(|target| target.to_string_lossy() == needle)
            {
                return Some(pid);
            }
        }
    }
    None
}

/// Resolves a PID to a process name.
fn process_name(pid: u32, os: Os) -> Option<String> {
    if os.is_windows() {
        return tasklist_process_name(pid);
    }
    let comm = std::fs::read_to_string(PathBuf::from("/proc").join(pid.to_string()).join("comm"))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    if let Some(name) = comm {
        return Some(name);
    }
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// Windows process name lookup through `tasklist`.
fn tasklist_process_name(pid: u32) -> Option<String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    tasklist_name(&String::from_utf8_lossy(&output.stdout), pid)
}

/// Parses `netstat -ano` output for the PID listening on `port`.
///
/// ```text
///   Proto  Local Address          Foreign Address        State           PID
///   TCP    0.0.0.0:3306           0.0.0.0:0              LISTENING       4240
/// ```
pub fn netstat_listener_pid(output: &str, port: u16) -> Option<u32> {
    let needle = format!(":{port}");
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let proto = fields.next()?.to_ascii_uppercase();
            if !proto.starts_with("TCP") {
                return None;
            }
            let local = fields.next()?;
            let foreign = fields.next()?;
            let state = fields.next()?;
            let pid = fields.next()?.parse::<u32>().ok()?;
            let local_port = local.rsplit(':').next()?;
            let foreign_port = foreign.rsplit(':').next()?;
            let listening = state.eq_ignore_ascii_case("LISTENING");
            if listening && local_port == needle.trim_start_matches(':') && foreign_port == "0" {
                Some(pid)
            } else {
                None
            }
        })
        .next()
}

/// Parses `lsof -nP -iTCP:<port> -sTCP:LISTEN` output for the PID.
///
/// ```text
/// COMMAND   PID  USER   FD   TYPE DEVICE SIZE/OFF NODE NAME
/// mysqld   4240 thio   21u  IPv4 0x1234      0t0  TCP *:3306 (LISTEN)
/// ```
pub fn lsof_listener_pid(output: &str, port: u16) -> Option<u32> {
    let needle = format!(":{port}");
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _command = fields.next()?;
            let pid = fields.next()?.parse::<u32>().ok()?;
            let matches_port = line
                .match_indices(&needle)
                // `:330` must not match inside `:3306`, so the character after
                // the port number has to end the number.
                .any(|(start, _)| {
                    let after = line[start + needle.len()..].chars().next();
                    after.is_none_or(|c| !c.is_ascii_digit())
                });
            (line.contains("(LISTEN)") && matches_port).then_some(pid)
        })
        .next()
}

/// Finds the socket inode of the listener on `port` in `/proc/net/tcp`.
///
/// Ports are hexadecimal in that file and the listening state is `0A`.
pub fn proc_net_tcp_inode(content: &str, port: u16) -> Option<u64> {
    let needle = format!(":{port:04X}");
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _slot = fields.next()?;
            let local = fields.next()?;
            let _foreign = fields.next()?;
            let state = fields.next()?;
            if state != "0A" || !local.ends_with(&needle) {
                return None;
            }
            // Field order: sl, local, rem, st, tx:rx, tr:tm, retrnsmt, uid,
            // timeout, inode.
            fields.nth(5)?.parse::<u64>().ok()
        })
        .next()
}

/// Parses `tasklist /FO CSV` output into a process name for `pid`.
pub fn tasklist_name(output: &str, pid: u32) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split(',');
        let name = fields.next()?.trim().trim_matches('"');
        let listed_pid = fields.next()?.trim().trim_matches('"');
        if listed_pid == pid.to_string() && !name.is_empty() {
            Some(name.to_owned())
        } else {
            None
        }
    })
}

/// Explains a port conflict for the user, with the options Lambo offers.
///
/// Used by `lambo up`, `lambo doctor` and `lambo db start`; keeping the text
/// in one place is what makes the three consistent.
pub fn conflict_advice(port: u16, occupied_by: Option<&str>, config_key: &str) -> String {
    let mut lines = Vec::new();
    match occupied_by {
        Some(name) => lines.push(format!("Port {port} is already in use by {name}.")),
        None => lines.push(format!("Port {port} is already in use.")),
    }
    lines.push(String::new());
    lines.push("Options:".to_owned());
    if occupied_by
        .map(|name| name.contains("mysql") || name.contains("maria"))
        .unwrap_or(false)
    {
        lines.push(
            "  1. Use the existing database (set the credentials with `lambo config set`)"
                .to_owned(),
        );
    }
    lines.push(format!(
        "  {}. Change the Lambo port: `lambo config set {config_key} {}`",
        if occupied_by
            .map(|name| name.contains("mysql") || name.contains("maria"))
            .unwrap_or(false)
        {
            2
        } else {
            1
        },
        alternatives(port).first().copied().unwrap_or(port + 1)
    ));
    lines.push(format!(
        "  {}. Stop the conflicting process yourself (Lambo never does that for you)",
        if occupied_by
            .map(|name| name.contains("mysql") || name.contains("maria"))
            .unwrap_or(false)
        {
            3
        } else {
            2
        }
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Binds an ephemeral port and returns it, keeping the listener alive.
    fn bound_port() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn a_bound_port_is_not_free() {
        let (_listener, port) = bound_port();
        assert!(!is_free(port), "port {port} should be reported as occupied");
        assert!(is_listening(port));
        assert!(check(port, Os::Linux).is_err());
    }

    #[test]
    fn an_unbound_port_is_free() {
        let (_listener, port) = bound_port();
        // The ephemeral port is released as soon as the listener is dropped,
        // and the OS will not hand it out again immediately, so a nearby port
        // is a safe choice for the negative case.
        let candidate = port + 1;
        if is_free(candidate) {
            assert!(check(candidate, Os::Linux).is_ok());
        }
    }

    #[test]
    fn first_free_skips_occupied_ports() {
        let (_listener, port) = bound_port();
        let found = first_free([port, port + 1, port + 2]).unwrap();
        assert_ne!(found, port);
        assert!(is_free(found));
    }

    #[test]
    fn alternatives_stay_unprivileged() {
        assert_eq!(alternatives(8080), vec![8081, 8082, 8083]);
        assert_eq!(alternatives(3306), vec![3307, 3308, 3309]);
        // A privileged port never gets a privileged suggestion.
        assert!(alternatives(80).iter().all(|port| *port > 1024));
        // u16::MAX must not overflow the loop.
        assert!(!alternatives(u16::MAX).is_empty());
        assert!(alternatives(u16::MAX).iter().all(|port| *port > 1024));
    }

    #[test]
    fn netstat_output_is_parsed() {
        let output = "\
  Proto  Local Address          Foreign Address        State           PID
  TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1204
  TCP    0.0.0.0:3306           0.0.0.0:0              LISTENING       4240
  TCP    127.0.0.1:3306         127.0.0.1:52100        ESTABLISHED     4240
  TCP    0.0.0.0:8080           0.0.0.0:0              LISTENING       512
";
        assert_eq!(netstat_listener_pid(output, 3306), Some(4240));
        assert_eq!(netstat_listener_pid(output, 8080), Some(512));
        assert_eq!(netstat_listener_pid(output, 13306), None);
        assert_eq!(netstat_listener_pid(output, 9999), None);
        assert_eq!(netstat_listener_pid("", 3306), None);
    }

    #[test]
    fn lsof_output_is_parsed() {
        let output = "\
COMMAND   PID   USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
mysqld   4240   thio   21u  IPv4 0xabcdef123456      0t0  TCP *:3306 (LISTEN)
httpd    512    thio   10u  IPv6 0xabcdef654321      0t0  TCP *:8080 (LISTEN)
";
        assert_eq!(lsof_listener_pid(output, 3306), Some(4240));
        assert_eq!(lsof_listener_pid(output, 8080), Some(512));
        assert_eq!(lsof_listener_pid(output, 5432), None);
        // A port that is only a prefix of the real one must not match.
        assert_eq!(lsof_listener_pid(output, 330), None);
        assert_eq!(lsof_listener_pid(output, 808), None);
        assert_eq!(lsof_listener_pid("COMMAND PID USER FD\n", 3306), None);
    }

    #[test]
    fn proc_net_tcp_is_parsed_in_hex() {
        let content = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000 100 0
   1: 00000000:0CEA 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 67890 1 0000 100 0
   2: 0100007F:1F91 0100007F:0CEA 01 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0000 100 0
";
        // 0x1F90 == 8080, 0x0CEA == 3306; only listening sockets match.
        assert_eq!(proc_net_tcp_inode(content, 8080), Some(12345));
        assert_eq!(proc_net_tcp_inode(content, 3306), Some(67890));
        assert_eq!(proc_net_tcp_inode(content, 8081), None);
    }

    #[test]
    fn tasklist_names_are_parsed() {
        let output = "\"mysqld.exe\",\"4240\",\"Console\",\"1\",\"412,000 K\"\n";
        assert_eq!(tasklist_name(output, 4240).as_deref(), Some("mysqld.exe"));
        assert_eq!(tasklist_name(output, 512), None);
        assert_eq!(
            tasklist_name(
                "INFO: No tasks are running which match the specified criteria.\n",
                4240
            ),
            None
        );
    }

    #[test]
    fn conflict_advice_names_the_occupant_and_the_fix() {
        let advice = conflict_advice(3306, Some("mysqld.exe (PID 4240)"), "database.port");
        assert!(advice.contains("mysqld.exe (PID 4240)"));
        assert!(advice.contains("lambo config set database.port 3307"));
        assert!(advice.contains("Lambo never does that for you"));

        let generic = conflict_advice(8080, None, "server.port");
        assert!(generic.contains("Port 8080 is already in use."));
        assert!(generic.contains("server.port 8081"));
    }
}
