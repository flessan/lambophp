//! Cross-platform process supervision.
//!
//! Lambo starts Apache, MariaDB and PHP's own server as *background*
//! processes that must outlive the `lambo` invocation that started them, must
//! be findable later, and must stop cleanly on `lambo down`. Getting that
//! right on Windows is where most XAMPP alternatives fall over, so the rules
//! here are explicit:
//!
//! - Services are started **detached**: on Windows with `DETACHED_PROCESS |
//!   CREATE_NEW_PROCESS_GROUP`, on Unix with `process_group(0)`. A service
//!   therefore never dies because the terminal that started it closed, and
//!   never shares a console with the CLI.
//! - Short-lived helper commands (`php -v`, `httpd -t`) are started with
//!   `CREATE_NO_WINDOW` on Windows so they do not flash a console window.
//! - Output of services is redirected to a log file, never inherited, so a
//!   detached process can never block on a closed console handle.
//! - Stopping is a two-step escalation: ask the service to shut down the way
//!   its own documentation describes, wait, then terminate the process tree.
//!   Nothing is ever killed blindly - see [`stop`].
//!
//! No shell is involved anywhere: every helper is invoked as an argument
//! vector, so no argument can ever be interpreted as shell syntax.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::platform::Os;

/// Windows process creation flags (winbase.h).
///
/// Declared locally so the crate needs no `windows` dependency for them.
// Referenced only by the Windows code path; kept defined on every platform so
// the constants stay documented and reviewable.
#[cfg_attr(not(windows), allow(dead_code))]
mod flags {
    /// The child does not inherit the parent's console.
    pub const DETACHED_PROCESS: u32 = 0x0000_0008;
    /// The child starts in its own process group, so `taskkill /T` and
    /// console control events can address it as a unit.
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    /// The child is a console application that should not create a window.
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
}

/// Where a spawned process writes its standard output or error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Inherit the parent's handle (interactive commands only).
    Inherit,
    /// Discard.
    Null,
    /// Append to a log file, creating it when needed.
    File(PathBuf),
}

impl Default for Output {
    fn default() -> Self {
        Self::Null
    }
}

/// A process to start, described platform-independently.
#[derive(Debug, Clone, Default)]
pub struct ProcessSpec {
    /// Executable to run.
    pub program: PathBuf,
    /// Arguments, in order. Never passed through a shell.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Where standard input comes from.
    pub stdin: Output,
    /// Where standard output goes.
    pub stdout: Output,
    /// Where standard error goes.
    pub stderr: Output,
    /// Start the process detached from this CLI invocation.
    pub detached: bool,
    /// Human-readable name used in messages and logs.
    pub name: String,
}

impl ProcessSpec {
    /// Describes a process to run.
    pub fn new(program: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            name: name.into(),
            ..Self::default()
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.args.push(argument.into());
        self
    }

    /// Appends several arguments.
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory.
    pub fn cwd(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cwd = Some(directory.into());
        self
    }

    /// Sets one environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Redirects standard output.
    pub fn stdout(mut self, output: Output) -> Self {
        self.stdout = output;
        self
    }

    /// Redirects standard error.
    pub fn stderr(mut self, output: Output) -> Self {
        self.stderr = output;
        self
    }

    /// Sends standard output and standard error to the same log file.
    pub fn log_to(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.stdout = Output::File(path.clone());
        self.stderr = Output::File(path);
        self
    }

    /// Marks the process as a long-running service.
    pub fn detached(mut self) -> Self {
        self.detached = true;
        self
    }

    /// Wires the process to this terminal (`lambo db shell`, `lambo php`).
    pub fn interactive(mut self) -> Self {
        self.stdin = Output::Inherit;
        self.stdout = Output::Inherit;
        self.stderr = Output::Inherit;
        self
    }

    /// Renders the invocation for log and status output.
    ///
    /// The program and any argument containing a space is quoted, which is what
    /// makes the recorded command line readable *and* re-runnable by a human.
    /// Quoting the program matters as much as quoting the arguments: the
    /// default Windows install lives under `C:\Program Files\...`, and an
    /// unquoted path breaks at the first space, so the command a user copies out
    /// of a log would not run.
    pub fn render(&self) -> String {
        let quote = |value: String| match value.contains(' ') {
            true => format!("\"{value}\""),
            false => value,
        };
        let mut parts = vec![quote(self.program.display().to_string())];
        parts.extend(self.args.iter().cloned().map(quote));
        parts.join(" ")
    }
}

/// Starts a process.
///
/// Returns the [`std::process::Child`] so callers can wait for exit or record
/// the PID. For services, drop the handle after recording the PID: the child
/// keeps running because it was started detached.
pub fn spawn(spec: &ProcessSpec, os: Os) -> Result<std::process::Child> {
    let mut command = build_command(spec, os)?;
    command.spawn().map_err(|source| Error::ServiceFailed {
        service: spec.name.clone(),
        reason: format!("could not start `{}`: {source}", spec.program.display()),
        causes: vec![
            "the executable is missing or not runnable".to_owned(),
            "antivirus software blocked the process".to_owned(),
            format!("run `lambo doctor` to inspect the installation"),
        ],
        hint: Some("lambo doctor".to_owned()),
    })
}

/// Starts a process and waits for it, capturing its output.
///
/// Used for short-lived helper commands such as `php -v`, `httpd -t` or
/// `mariadb --execute`.
pub fn run(spec: &ProcessSpec, os: Os) -> Result<std::process::Output> {
    let mut command = build_command(spec, os)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.output().map_err(|source| Error::ServiceFailed {
        service: spec.name.clone(),
        reason: format!("could not run `{}`: {source}", spec.program.display()),
        causes: vec!["the executable is missing or not runnable".to_owned()],
        hint: Some("lambo doctor".to_owned()),
    })
}

/// Turns a [`ProcessSpec`] into a platform-configured [`Command`].
///
/// Split out from [`spawn`] so the platform-specific parts (creation flags,
/// process groups, stdio wiring) can be reasoned about in one place.
fn build_command(spec: &ProcessSpec, os: Os) -> Result<Command> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);

    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    command.stdin(stdio_for(&spec.stdin)?);
    command.stdout(stdio_for(&spec.stdout)?);
    command.stderr(stdio_for(&spec.stderr)?);

    apply_platform_flags(&mut command, spec, os);
    Ok(command)
}

/// Opens the stdio handle described by `output`.
fn stdio_for(output: &Output) -> Result<Stdio> {
    match output {
        Output::Inherit => Ok(Stdio::inherit()),
        Output::Null => Ok(Stdio::null()),
        Output::File(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
            }
            let file = File::options()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| Error::io(path, source))?;
            Ok(Stdio::from(file))
        }
    }
}

/// Applies the platform-specific process flags.
#[cfg(windows)]
fn apply_platform_flags(command: &mut Command, spec: &ProcessSpec, _os: Os) {
    use std::os::windows::process::CommandExt;
    let flags = if spec.detached {
        flags::DETACHED_PROCESS | flags::CREATE_NEW_PROCESS_GROUP
    } else {
        flags::CREATE_NO_WINDOW
    };
    command.creation_flags(flags);
}

/// Applies the platform-specific process flags.
#[cfg(not(windows))]
fn apply_platform_flags(command: &mut Command, spec: &ProcessSpec, _os: Os) {
    use std::os::unix::process::CommandExt;
    if spec.detached {
        // A new session detaches the child from the controlling terminal, so
        // closing the terminal (or Ctrl+C in it) does not take services down.
        command.process_group(0);
    }
}

/// Whether a process with this PID is alive.
pub fn is_running(pid: u32, os: Os) -> bool {
    if os.is_windows() {
        is_running_windows(pid)
    } else {
        is_running_unix(pid)
    }
}

/// Windows liveness check through `tasklist`.
fn is_running_windows(pid: u32) -> bool {
    let Ok(output) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
    else {
        return false;
    };
    tasklist_reports_pid(&String::from_utf8_lossy(&output.stdout), pid)
}

/// Unix liveness check: `/proc` when available, otherwise `kill -0`.
///
/// A process that has exited but not yet been reaped still has a `/proc`
/// entry, so its state has to be read: a zombie (`Z`) is dead for every
/// purpose Lambo cares about, and reporting it as alive would make
/// `lambo down` escalate to `kill -9` on a process that no longer exists.
fn is_running_unix(pid: u32) -> bool {
    let proc_entry = PathBuf::from("/proc").join(pid.to_string());
    if Path::new("/proc").is_dir() {
        if !proc_entry.is_dir() {
            return false;
        }
        return !is_zombie(&proc_entry);
    }
    Command::new(kill_program())
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Whether `/proc/<pid>` describes a zombie.
///
/// The state field follows the command name, which may itself contain spaces
/// and parentheses, so the split happens at the *last* `") "`.
fn is_zombie(proc_entry: &Path) -> bool {
    let Ok(stat) = std::fs::read_to_string(proc_entry.join("stat")) else {
        return false;
    };
    process_state(&stat).is_some_and(|state| state == 'Z')
}

/// Extracts the single-character state field from `/proc/<pid>/stat`.
pub fn process_state(stat: &str) -> Option<char> {
    let after_name = stat.rsplit_once(") ")?.1;
    after_name.split_whitespace().next()?.chars().next()
}

/// Parses `tasklist` CSV output, which looks like:
///
/// ```text
/// "httpd.exe","4240","Console","1","12,345 K"
/// ```
pub fn tasklist_reports_pid(output: &str, pid: u32) -> bool {
    output.lines().any(|line| {
        line.split(',')
            .nth(1)
            .map(|field| field.trim_matches('"').trim() == pid.to_string())
            .unwrap_or(false)
    })
}

/// What identifies a process well enough to tell it apart from an unrelated
/// one that later reused its PID.
///
/// A PID on its own is not identity: it is a number Lambo read at some point in
/// the past, and every operating system hands PIDs back out. Killing whatever
/// holds that number now is how a development tool ends up shutting down
/// somebody else's editor. The start time is what makes the check real - no
/// reused PID can share the original process's boot-relative start time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// The process identifier this was read for.
    pub pid: u32,
    /// Start time in clock ticks since boot (Unix) or as an opaque token.
    ///
    /// `None` when the platform offers no way to read it; identity then falls
    /// back to the executable name, which is weaker but still catches the
    /// common case.
    pub start_time: Option<u64>,
    /// The image name, e.g. `httpd` or `mariadbd.exe`.
    pub executable: Option<String>,
}

impl ProcessIdentity {
    /// Whether a freshly probed identity still describes the same process.
    ///
    /// Compares only what both readings have: a field that could not be read on
    /// either side is not evidence of a change. If nothing comparable survives,
    /// the answer is "cannot tell", and callers must not kill.
    pub fn matches(&self, other: &ProcessIdentity) -> bool {
        if self.pid != other.pid {
            return false;
        }
        let mut compared = false;
        if let (Some(a), Some(b)) = (self.start_time, other.start_time) {
            if a != b {
                return false;
            }
            compared = true;
        }
        if let (Some(a), Some(b)) = (&self.executable, &other.executable) {
            if !a.eq_ignore_ascii_case(b) {
                return false;
            }
            compared = true;
        }
        compared
    }
}

/// Reads the identity of a process Lambo has just started, waiting for the
/// `exec` that turns it into the program that was asked for.
///
/// A child exists twice: first as a fork of Lambo, still carrying Lambo's own
/// image name, and again after `exec` replaces it with the real program.
/// Probing inside that window records the *parent's* name. The start time is
/// already final at fork, so nothing else looks wrong - but the name never
/// matches a later probe, `matches` returns false, and `lambo down` concludes
/// the PID belongs to somebody else. It signals nothing and removes the record,
/// leaving Lambo's own service running with no way to stop it. That is the
/// exact failure the identity check exists to prevent.
///
/// If the name still has not settled by the deadline, the executable is dropped
/// rather than recorded. A missing name is only a weaker comparison - the start
/// time still identifies the process - whereas a *wrong* name actively proves
/// a falsehood and makes the service unremovable.
pub fn identity_settled(
    pid: u32,
    os: Os,
    expected: &Path,
    timeout: Duration,
) -> Option<ProcessIdentity> {
    let wanted = expected
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let deadline = Instant::now() + timeout;

    loop {
        let current = identity(pid, os)?;
        if let (Some(wanted), Some(actual)) = (&wanted, &current.executable) {
            // Linux truncates `comm` to 15 bytes, so a longer program name can
            // only ever match its own prefix.
            let truncated: String = wanted.chars().take(15).collect();
            if actual.eq_ignore_ascii_case(wanted) || actual.eq_ignore_ascii_case(&truncated) {
                return Some(current);
            }
        } else {
            // Nothing to compare against, so this reading is as good as it gets.
            return Some(current);
        }
        if Instant::now() >= deadline {
            return Some(ProcessIdentity {
                executable: None,
                ..current
            });
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Reads the identity of a live process, or `None` if it is gone.
pub fn identity(pid: u32, os: Os) -> Option<ProcessIdentity> {
    if os.is_windows() {
        identity_windows(pid)
    } else {
        identity_unix(pid)
    }
}

/// Unix identity from `/proc/<pid>/stat` and `/proc/<pid>/comm`.
fn identity_unix(pid: u32) -> Option<ProcessIdentity> {
    let entry = PathBuf::from("/proc").join(pid.to_string());
    let stat = std::fs::read_to_string(entry.join("stat")).ok()?;
    let comm = std::fs::read_to_string(entry.join("comm"))
        .ok()
        .map(|name| name.trim().to_owned());

    // Field 22 is the start time in clock ticks since boot. The command name in
    // field 2 is parenthesised and may itself contain spaces and parentheses,
    // so the split happens at the last `") "` - the same rule `process_state`
    // uses. What follows starts at field 3, so field 22 is index 19.
    let start_time = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|field| field.parse::<u64>().ok());

    Some(ProcessIdentity {
        pid,
        start_time,
        executable: comm,
    })
}

/// Windows identity from the same `tasklist` call the liveness check makes.
///
/// Windows exposes no cheap boot-relative start time without a heavier API, so
/// the image name carries the check here.
fn identity_windows(pid: u32) -> Option<ProcessIdentity> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !tasklist_reports_pid(&text, pid) {
        return None;
    }
    let executable = text
        .lines()
        .find_map(|line| {
            line.split(',')
                .next()
                .map(|field| field.trim_matches('"').trim().to_owned())
        })
        .filter(|name| !name.is_empty());
    Some(ProcessIdentity {
        pid,
        start_time: None,
        executable,
    })
}

/// Outcome of stopping a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// It was not running to begin with.
    AlreadyGone,
    /// The PID is held by a different process than the one Lambo started.
    ///
    /// Nothing was signalled. The record is stale and should be dropped.
    NotOurs,
    /// It shut down after the graceful request.
    Graceful,
    /// It had to be terminated.
    Forced,
    /// It is still running after both attempts.
    Failed(String),
}

impl StopOutcome {
    /// Whether the process is gone.
    pub fn stopped(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

/// Stops a process, escalating from graceful to forced.
///
/// `graceful` is the service's own shutdown command (Apache's `httpd -k
/// stop`, MariaDB's `mariadb-admin shutdown`) when it has one. Lambo only
/// reaches for `taskkill` / `kill` afterwards; see [`terminate_tree`] for what
/// each platform targets and why.
/// Stops a process Lambo started, after confirming it is still the same one.
///
/// `expected` is the identity recorded when the service was launched. If the
/// PID has been handed to an unrelated process, nothing is signalled and
/// [`StopOutcome::NotOurs`] is returned: terminating the wrong process is
/// unrecoverable, while leaving a stale record behind is merely untidy.
///
/// A record with no identity - written by an older Lambo - falls back to
/// [`stop`], which is the previous behaviour and the best available answer.
pub fn stop_verified(
    pid: u32,
    expected: Option<&ProcessIdentity>,
    os: Os,
    graceful: Option<&ProcessSpec>,
    timeout: Duration,
) -> Result<StopOutcome> {
    if !is_running(pid, os) {
        return Ok(StopOutcome::AlreadyGone);
    }
    if let Some(expected) = expected {
        match identity(pid, os) {
            Some(actual) if expected.matches(&actual) => {}
            // Either the probe failed or it describes a different process.
            // Both mean "do not touch this PID".
            _ => return Ok(StopOutcome::NotOurs),
        }
    }
    stop(pid, os, graceful, timeout)
}

pub fn stop(
    pid: u32,
    os: Os,
    graceful: Option<&ProcessSpec>,
    timeout: Duration,
) -> Result<StopOutcome> {
    if !is_running(pid, os) {
        return Ok(StopOutcome::AlreadyGone);
    }

    if let Some(spec) = graceful {
        // A failing shutdown command is not fatal: the escalation below
        // still runs, and the service log carries the reason.
        let _ = run(spec, os);
        if wait_until_gone(pid, os, timeout) {
            return Ok(StopOutcome::Graceful);
        }
    }

    terminate_tree(pid, os, false)?;
    if wait_until_gone(pid, os, timeout) {
        return Ok(StopOutcome::Forced);
    }

    terminate_tree(pid, os, true)?;
    if wait_until_gone(pid, os, timeout) {
        return Ok(StopOutcome::Forced);
    }

    Ok(StopOutcome::Failed(format!(
        "process {pid} is still running; close it manually and run `lambo doctor`"
    )))
}

/// Requests termination of a service and anything it started.
///
/// `force` selects the polite or the unconditional form.
///
/// # Why Windows and Unix differ
///
/// **Windows** gets `taskkill /PID <pid> /T`, which the operating system scopes
/// to that process and its descendants. It cannot reach an unrelated process,
/// and it is the only reliable way to stop Apache's parent *and* its worker -
/// which is what "no orphans after `lambo down`" requires.
///
/// **Unix** gets a signal to the bare PID, never to a process group. Signalling
/// a group (`kill -TERM -<pgid>`) is tempting because services are started with
/// [`process_group(0)`], but it is the wrong tool:
///
/// - The PID in the state file is a number Lambo read earlier. If the process
///   exited and the PID was reused, the group it now leads belongs to somebody
///   else, and a group signal takes *all* of it down. A bare-PID signal kills
///   one process, which is recoverable; a group signal is not.
/// - Restricted environments refuse or, worse, reinterpret group signals, and
///   the failure mode is invisible: `kill` still exits successfully.
/// - It is also unnecessary. Every service Lambo manages shuts its own
///   children down when its main process is asked to stop - Apache's parent
///   terminates its workers, and `mysqld`/`mariadbd` and PHP's built-in server
///   have no children at all. The graceful command runs first anyway.
pub fn terminate_tree(pid: u32, os: Os, force: bool) -> Result<()> {
    if os.is_windows() {
        let mut command = Command::new("taskkill");
        command.arg("/PID").arg(pid.to_string()).arg("/T");
        if force {
            command.arg("/F");
        }
        command.stdout(Stdio::null()).stderr(Stdio::null());
        // `taskkill` exits non-zero when the process already vanished, which
        // is exactly what the caller wants to treat as success.
        let _ = command.status();
        return Ok(());
    }

    let signal = if force { "-KILL" } else { "-TERM" };
    signal_pid(&pid.to_string(), signal);
    Ok(())
}

/// Runs `kill <signal> <target>`, ignoring the exit status.
///
/// `kill` exits non-zero when the target has already disappeared, which is
/// the outcome the caller wants anyway.
fn signal_pid(target: &str, signal: &str) {
    let _ = Command::new(kill_program())
        .args([signal, target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Waits until `pid` disappears or the timeout elapses.
pub fn wait_until_gone(pid: u32, os: Os, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_running(pid, os) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !is_running(pid, os)
}

/// Waits until `pid` is *alive*, used after starting a service.
pub fn wait_until_running(pid: u32, os: Os, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_running(pid, os) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    is_running(pid, os)
}

/// The `kill` utility to use on Unix.
///
/// `/bin/kill` exists on Linux (util-linux) and macOS; the bare name is the
/// fallback for distributions that place it elsewhere. Lambo invokes it as a
/// program, not through a shell.
pub fn kill_program() -> &'static str {
    if Path::new("/bin/kill").exists() {
        "/bin/kill"
    } else {
        "kill"
    }
}

/// Looks up a program on `PATH` plus the platform's conventional locations.
///
/// Returns the first existing candidate. Used to find a system PHP, Apache or
/// MySQL when the user already has one installed.
pub fn find_program(name: &str, os: Os) -> Option<PathBuf> {
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    directories.extend(os.program_search_dirs());
    find_program_in(name, os, &directories)
}

/// Directory-list variant of [`find_program`].
///
/// The search path is an argument so the lookup can be tested without
/// mutating the process environment - which matters twice over, because
/// `std::env::set_var` is `unsafe` in edition 2024 and this crate forbids
/// `unsafe` outright.
pub fn find_program_in(name: &str, os: Os, directories: &[PathBuf]) -> Option<PathBuf> {
    let executable = os.executable_name(name);
    directories
        .iter()
        .map(|directory| directory.join(&executable))
        .find(|candidate| crate::runtime::is_executable_file(candidate, os))
}

/// Arguments that are safe to pass to a helper program.
///
/// Lambo never builds command lines by string concatenation, but database
/// names and passwords do end up in argument vectors; this rejects the
/// characters that would make such an argument ambiguous in a `.env` file or
/// a log line.
pub fn is_safe_argument(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '\n' | '\r'))
}

/// Builds an [`OsString`] argument list for logging.
pub fn render_arguments(args: &[String]) -> OsString {
    let mut rendered = OsString::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            rendered.push(" ");
        }
        rendered.push(argument);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A program that stays alive long enough to be inspected.
    fn sleeper(seconds: u32) -> ProcessSpec {
        #[cfg(windows)]
        {
            ProcessSpec::new(r"C:\Windows\System32\ping.exe", "sleeper")
                .arg("-n")
                .arg((seconds + 1).to_string())
                .arg("127.0.0.1")
        }
        #[cfg(not(windows))]
        {
            ProcessSpec::new("sleep", "sleeper").arg(seconds.to_string())
        }
    }

    #[test]
    fn the_recorded_identity_describes_the_child_and_not_lambo() {
        let os = Os::host();
        let spec = sleeper(5);
        let child = spawn(&spec, os).unwrap();
        let pid = child.id();

        // Reading the identity straight after spawn is what every start path
        // used to do, and on Unix it can land before `exec` replaces the forked
        // copy of Lambo - recording Lambo's own name instead of the program
        // that was launched. Settling first is the whole point.
        let settled = identity_settled(pid, os, &spec.program, Duration::from_secs(5))
            .expect("the child is running");

        // Deterministic half of the check: the name is either the program that
        // was launched, or absent. Any other value is Lambo's own image name
        // read before `exec`, which would make the service unremovable.
        let expected = spec
            .program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        if let (Some(expected), Some(actual)) = (&expected, &settled.executable) {
            let truncated: String = expected.chars().take(15).collect();
            assert!(
                actual.eq_ignore_ascii_case(expected) || actual.eq_ignore_ascii_case(&truncated),
                "recorded `{actual}` but launched `{expected}`"
            );
        }

        // And a later probe must still agree, which is what `lambo down` relies
        // on before it is willing to signal anything.
        let later = identity(pid, os).expect("still running");
        assert!(
            settled.matches(&later),
            "the recorded identity {settled:?} does not match a later probe {later:?}; \
             `lambo down` would refuse to stop this service and leave it running"
        );

        // Reaped rather than dropped, so the test leaves no zombie behind.
        let outcome = stop(pid, os, None, Duration::from_secs(5)).unwrap();
        assert!(outcome.stopped(), "{outcome:?}");
        drop(child);
    }

    #[test]
    fn spec_rendering_quotes_arguments_with_spaces() {
        let spec = ProcessSpec::new(r"C:\Lambo\apache\2.4.62\bin\httpd.exe", "apache")
            .arg("-f")
            .arg(r"C:\Lambo\My Config\httpd.conf")
            .arg("-k")
            .arg("start");
        assert_eq!(
            spec.render(),
            r#"C:\Lambo\apache\2.4.62\bin\httpd.exe -f "C:\Lambo\My Config\httpd.conf" -k start"#
        );
    }

    #[test]
    fn output_files_are_created_with_parents() {
        let temp = TempDir::new();
        let log = temp.path().join("logs/apache/httpd.log");
        let spec = ProcessSpec::new(program_that_writes(), "writer")
            .args(writer_arguments("hello from lambo"))
            .log_to(&log);

        let mut child = spawn(&spec, Os::host()).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());

        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            contents.contains("hello from lambo"),
            "log contained: {contents}"
        );
    }

    #[test]
    fn detached_processes_are_found_and_stopped() {
        let os = Os::host();
        let spec = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut child = spawn(&spec, os).unwrap();
        let pid = child.id();

        assert!(
            wait_until_running(pid, os, Duration::from_secs(5)),
            "service did not start"
        );
        assert!(is_running(pid, os));

        let outcome = stop(pid, os, None, Duration::from_secs(10)).unwrap();
        assert!(outcome.stopped(), "stop reported {outcome:?}");
        assert!(!is_running(pid, os), "the process is still alive");

        // Stopping something that is already gone is not an error.
        let again = stop(pid, os, None, Duration::from_secs(1)).unwrap();
        assert_eq!(again, StopOutcome::AlreadyGone);

        let _ = child.wait();
    }

    #[test]
    fn stopping_one_service_leaves_every_other_process_alone() {
        // The safety property behind `terminate_tree`: Lambo signals a PID it
        // recorded, never a process group, so a service that happens to share a
        // group with something else cannot take it down. Two detached services
        // stand in for "mine" and "somebody else's".
        let os = Os::host();
        let spec = || {
            sleeper(30)
                .detached()
                .stdout(Output::Null)
                .stderr(Output::Null)
        };
        let mut mine = spawn(&spec(), os).unwrap();
        let mut theirs = spawn(&spec(), os).unwrap();
        let (mine_pid, theirs_pid) = (mine.id(), theirs.id());
        assert!(wait_until_running(mine_pid, os, Duration::from_secs(5)));
        assert!(wait_until_running(theirs_pid, os, Duration::from_secs(5)));

        let outcome = stop(mine_pid, os, None, Duration::from_secs(10)).unwrap();
        assert!(outcome.stopped(), "{outcome:?}");

        assert!(!is_running(mine_pid, os), "the service must be gone");
        assert!(
            is_running(theirs_pid, os),
            "an unrelated process must survive"
        );

        let _ = stop(theirs_pid, os, None, Duration::from_secs(10)).unwrap();
        let _ = mine.wait();
        let _ = theirs.wait();
    }

    #[test]
    fn a_live_process_reports_a_stable_identity() {
        let os = Os::host();
        let spec = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut child = spawn(&spec, os).unwrap();
        let pid = child.id();
        assert!(wait_until_running(pid, os, Duration::from_secs(5)));

        let first = identity(pid, os).expect("a running process has an identity");
        let second = identity(pid, os).expect("and it can be read again");
        assert_eq!(first.pid, pid);
        assert!(
            first.matches(&second),
            "the same process must read as itself: {first:?} vs {second:?}"
        );

        let _ = stop(pid, os, None, Duration::from_secs(10)).unwrap();
        let _ = child.wait();
    }

    #[test]
    fn a_reused_pid_is_not_terminated() {
        // The safety property behind `stop_verified`. Lambo recorded a PID and
        // an identity; the process exited and the PID now belongs to something
        // else. Signalling it would kill an unrelated program, so the answer
        // must be "not ours" and the live process must survive untouched.
        let os = Os::host();
        let spec = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut unrelated = spawn(&spec, os).unwrap();
        let pid = unrelated.id();
        assert!(wait_until_running(pid, os, Duration::from_secs(5)));

        // An identity that describes a different program entirely.
        let stale = ProcessIdentity {
            pid,
            start_time: None,
            executable: Some("definitely-not-this-process".to_owned()),
        };

        let outcome = stop_verified(pid, Some(&stale), os, None, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, StopOutcome::NotOurs, "{outcome:?}");
        assert!(
            is_running(pid, os),
            "the unrelated process must still be alive"
        );

        let _ = stop(pid, os, None, Duration::from_secs(10)).unwrap();
        let _ = unrelated.wait();
    }

    #[test]
    fn a_matching_identity_stops_the_process_normally() {
        // The other half: the check must not turn into a refusal to ever stop
        // anything. A genuine Lambo service still shuts down.
        let os = Os::host();
        let spec = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut child = spawn(&spec, os).unwrap();
        let pid = child.id();
        assert!(wait_until_running(pid, os, Duration::from_secs(5)));

        let recorded = identity(pid, os).expect("identity at start-up");
        let outcome =
            stop_verified(pid, Some(&recorded), os, None, Duration::from_secs(10)).unwrap();
        assert!(outcome.stopped(), "{outcome:?}");
        assert_ne!(
            outcome,
            StopOutcome::NotOurs,
            "our own service must be stopped"
        );
        assert!(!is_running(pid, os), "the service must be gone");

        let _ = child.wait();
    }

    #[test]
    fn an_identity_that_cannot_be_compared_never_authorises_a_kill() {
        // A record carrying nothing comparable is not a licence to signal: the
        // honest answer is "cannot tell", which must resolve to NotOurs rather
        // than to a kill.
        let os = Os::host();
        let spec = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut unrelated = spawn(&spec, os).unwrap();
        let pid = unrelated.id();
        assert!(wait_until_running(pid, os, Duration::from_secs(5)));

        let empty = ProcessIdentity {
            pid,
            start_time: None,
            executable: None,
        };
        assert!(
            !empty.matches(&identity(pid, os).unwrap()),
            "an empty identity must not match anything"
        );

        let outcome = stop_verified(pid, Some(&empty), os, None, Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, StopOutcome::NotOurs, "{outcome:?}");
        assert!(is_running(pid, os), "nothing comparable, nothing signalled");

        let _ = stop(pid, os, None, Duration::from_secs(10)).unwrap();
        let _ = unrelated.wait();
    }

    #[test]
    fn graceful_shutdown_command_is_used_when_provided() {
        let os = Os::host();
        let service = sleeper(30)
            .detached()
            .stdout(Output::Null)
            .stderr(Output::Null);
        let mut child = spawn(&service, os).unwrap();
        let pid = child.id();
        assert!(wait_until_running(pid, os, Duration::from_secs(5)));

        // A "graceful" command that kills the process stands in for
        // `httpd -k stop` / `mariadb-admin shutdown`, which cannot be
        // exercised without a real server installed.
        let graceful = killer(pid);
        let outcome = stop(pid, os, Some(&graceful), Duration::from_secs(10)).unwrap();
        assert!(
            matches!(outcome, StopOutcome::Graceful | StopOutcome::Forced),
            "{outcome:?}"
        );
        assert!(!is_running(pid, os));
        // Reap the child so the test leaves no zombie behind.
        let _ = child.wait();
    }

    #[test]
    fn liveness_of_unknown_pids_is_false() {
        // PID 0 and an absurdly large PID are never ours.
        assert!(!is_running(u32::MAX, Os::Linux));
        assert!(!is_running(u32::MAX, Os::Windows));
    }

    #[test]
    fn tasklist_output_is_parsed_by_pid_column() {
        let output = "\"httpd.exe\",\"4240\",\"Console\",\"1\",\"12,345 K\"\n\
                      \"mysqld.exe\",\"512\",\"Console\",\"1\",\"400,000 K\"\n";
        assert!(tasklist_reports_pid(output, 4240));
        assert!(tasklist_reports_pid(output, 512));
        assert!(!tasklist_reports_pid(output, 424));
        assert!(!tasklist_reports_pid(output, 42400));
        assert!(!tasklist_reports_pid(
            "INFO: No tasks are running which match the specified criteria.",
            4240
        ));
        assert!(!tasklist_reports_pid("", 1));
    }

    #[test]
    fn proc_stat_state_is_parsed_even_with_odd_command_names() {
        assert_eq!(
            process_state("5927 (sleep) S 1 5927 5927 0 -1 4194560"),
            Some('S')
        );
        assert_eq!(
            process_state("5927 (a (weird) name) Z 1 5927 5927 0 -1 4194560"),
            Some('Z')
        );
        assert_eq!(process_state("garbage"), None);
    }

    #[test]
    fn unsafe_arguments_are_rejected() {
        assert!(is_safe_argument("shop_dev"));
        assert!(is_safe_argument("-u"));
        assert!(!is_safe_argument(""));
        assert!(!is_safe_argument("a\nb"));
        assert!(!is_safe_argument("a\u{0}b"));
    }

    #[test]
    fn find_program_finds_something_that_exists() {
        // `lambo doctor` needs this for system installations; the exact
        // result is machine-dependent, so only the contract is asserted.
        let php = find_program("definitely-not-a-real-program-xyz", Os::host());
        assert!(php.is_none());

        let temp = TempDir::new();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join(Os::host().executable_name("lambo-test-tool"));
        std::fs::write(&program, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&program).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&program, permissions).unwrap();
        }

        assert_eq!(
            find_program_in("lambo-test-tool", Os::host(), &[bin]).as_deref(),
            Some(program.as_path())
        );
        // An unrelated directory must not produce a hit.
        assert!(
            find_program_in("lambo-test-tool", Os::host(), &[temp.path().to_path_buf()]).is_none()
        );
    }

    /// A program that writes its arguments to standard output.
    fn program_that_writes() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(r"C:\Windows\System32\cmd.exe")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/bin/echo")
        }
    }

    /// Arguments that make [`program_that_writes`] print `text`.
    fn writer_arguments(text: &str) -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["/C".to_owned(), "echo".to_owned(), text.to_owned()]
        }
        #[cfg(not(windows))]
        {
            vec![text.to_owned()]
        }
    }

    /// A command that terminates `pid`, standing in for a service's own
    /// shutdown command.
    fn killer(pid: u32) -> ProcessSpec {
        #[cfg(windows)]
        {
            ProcessSpec::new("taskkill", "killer").args(["/PID", &pid.to_string(), "/F"])
        }
        #[cfg(not(windows))]
        {
            ProcessSpec::new(kill_program(), "killer").args(["-KILL", &pid.to_string()])
        }
    }
}
