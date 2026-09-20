//! The service engine: one supervised process, as `service.go` had it.
//!
//! Every long-running program Lambo starts - Apache, nginx, PHP-FPM, MariaDB,
//! Redis, PostgreSQL, the Node helper - goes through [`Service`]. It exists so
//! that the *rules* around a supervised process are written once:
//!
//! * a service that is already running is not started twice, and one whose port
//!   is taken is not started at all,
//! * standard output and error are streamed into the log line by line, with the
//!   service's name in front of every line,
//! * the state callback fires on start, on exit and on a failed start,
//! * a stop asks the service's own tool first (`pg_ctl` for PostgreSQL) and
//!   falls back to terminating the process *tree*, then to the bare process,
//! * PostgreSQL is launched through the `runas` trick described below.
//!
//! # The split
//!
//! None of those rules is about Windows, so none of them is behind a `cfg`.
//! Everything platform-shaped - spawning with the console suppressed, the
//! `taskkill` tree kill, probing a port, waiting for a process - is a
//! [`ServiceHost`]. [`HostService`] is the real implementation, on top of
//! [`crate::process`]; the tests drive a scripted host instead, which is what
//! makes the PostgreSQL path - the most intricate part of the original - a unit
//! test rather than something only a Windows machine with a database installed
//! can exercise.
//!
//! # PostgreSQL
//!
//! `postgres.exe` refuses to run as an administrator, and the control panel may
//! well *be* elevated. The original's answer, kept here: instead of starting
//! PostgreSQL directly, it starts `runas /trustlevel:0x20000`, which runs the
//! command line at a restricted token level. Because that intermediary returns
//! immediately, the engine cannot learn the server's PID from it - so it reads
//! `<data dir>\postmaster.pid`, which PostgreSQL writes itself, and then waits
//! on that PID. Until the file appears, the service holds a placeholder handle
//! pointing at *this* process, which is what the original did: the handle is
//! the mutex that says "this service is running", and waiting on it is how the
//! wait loop would end if the interface itself went away.
//!
//! `postgres.exe` also writes its log where its configuration says, so the
//! engine finds the file (`current_logfiles` first, the newest `.log` in
//! `log\` second) and follows it, rewriting `2026-01-01 00:00:00 GMT [123]
//! LOG:  message` into `LOG: message`.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::logs::{LogFn, nop_log};
use crate::process::ProcessSpec;

/// The internal flag that makes this executable run a program and exit.
///
/// The Windows service launcher looks for it before anything else does.
pub const HIDE_RUN_FLAG: &str = "--hide-run";

/// The service name that selects the PostgreSQL path.
pub const POSTGRES: &str = "PostgreSQL";

/// PostgreSQL's own record of the running server: its first line is the PID.
pub const POSTMASTER_PID_FILE: &str = "postmaster.pid";

/// PostgreSQL's list of the log files it is currently writing.
pub const CURRENT_LOGFILES: &str = "current_logfiles";

/// The subdirectory PostgreSQL's logs live in by default.
pub const LOG_SUBDIR: &str = "log";

/// The extension a PostgreSQL log file has.
pub const LOG_EXTENSION: &str = "log";

/// The tool PostgreSQL ships for stopping a server cleanly.
pub const PG_CTL_PROGRAM: &str = "pg_ctl.exe";

/// How many times the engine looks for `postmaster.pid` before giving up.
pub const PID_POLL_ATTEMPTS: usize = 10;

/// How long it waits between those attempts.
pub const PID_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How many times the engine looks for PostgreSQL's log file.
pub const LOG_LOOKUP_ATTEMPTS: usize = 20;

/// How long it waits between those attempts.
pub const LOG_LOOKUP_INTERVAL: Duration = Duration::from_millis(200);

/// How long a port probe pauses before reporting the port free.
///
/// The original slept 10 ms after a successful bind: a listening socket that was
/// just released is not immediately bindable again on Windows, and a service
/// restart would otherwise fail against its own predecessor.
pub const PORT_PROBE_PAUSE: Duration = Duration::from_millis(10);

/// How long the log follower waits before asking a quiet file again.
pub const LOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What running a helper process produced.
///
/// `combined` is what the original's `CombinedOutput` returned and is what a
/// failure message quotes; `stdout` is kept separately because a program's
/// *output* is not its diagnostics, and callers that read the output need it
/// without the errors mixed in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedRun {
    /// Whether the program exited successfully.
    pub success: bool,
    /// The exit code, when it reported one.
    pub code: Option<i32>,
    /// Standard output, as written.
    pub stdout: String,
    /// Standard error, as written.
    pub stderr: String,
    /// Both streams, the way `CombinedOutput` returned them.
    pub combined: String,
}

impl CapturedRun {
    /// The exit status in the original's words.
    pub fn exit_reason(&self) -> String {
        match self.code {
            Some(code) => format!("exit status {code}"),
            None => "no exit status (killed?)".to_owned(),
        }
    }
}

/// How a supervised process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// It exited successfully.
    Clean,
    /// It ended badly: the reason is the original's `%v` for the error.
    Failed(String),
}

/// A process this engine supervises.
///
/// The handle is what identifies the service as running and what the wait loop
/// blocks on. Everything else - terminating it, finding its PID again - is
/// done by PID, which is what `taskkill` takes and what the original used.
pub trait HostedProcess: Send + Sync {
    /// Its process id.
    fn pid(&self) -> u32;

    /// Waits for it to end.
    fn wait(&self) -> WaitOutcome;

    /// Takes the pipe holding its standard output, if there is one.
    fn take_stdout(&self) -> Option<Box<dyn Read + Send>>;

    /// Takes the pipe holding its standard error, if there is one.
    fn take_stderr(&self) -> Option<Box<dyn Read + Send>>;
}

/// Everything the engine needs from the operating system.
///
/// A trait rather than free functions because the interesting behaviour is the
/// *decisions* - which of the three stop paths runs, what is logged when a
/// start fails, when the state callback fires - and those must be testable
/// without a database, a port and a process tree to hand.
pub trait ServiceHost: Send + Sync + 'static {
    /// Whether something is listening on `port` on the loopback interface.
    ///
    /// The original bound the port to find out, so a port that is free to bind
    /// is free to use.
    fn port_busy(&self, port: u16) -> bool;

    /// Starts `spec`.
    ///
    /// `piped` asks for standard output and error as pipes, which is what a
    /// service's log needs; without it the child inherits this process's
    /// handles, which is what the `runas` intermediary does.
    fn start(&self, spec: &ProcessSpec, piped: bool) -> Result<Arc<dyn HostedProcess>>;

    /// A handle on the process with `pid`.
    ///
    /// No error: the original ignored the failure too, and a handle whose waits
    /// fail immediately is the outcome it got.
    fn process_handle(&self, pid: u32) -> Arc<dyn HostedProcess>;

    /// Terminates a process and everything it started.
    fn kill_tree(&self, pid: u32) -> Result<()>;

    /// Terminates one process, the fallback when [`ServiceHost::kill_tree`]
    /// fails.
    fn kill_process(&self, pid: u32) -> Result<()>;

    /// Runs a short-lived program and captures what it said.
    fn run_captured(&self, spec: &ProcessSpec) -> Result<CapturedRun>;

    /// This executable, for the `runas` command line.
    fn self_exe(&self) -> Result<PathBuf>;

    /// Waits. Split out so a test does not have to.
    fn sleep(&self, duration: Duration);

    /// Every process of this machine that has a path.
    ///
    /// Used by the startup sweep (`crate::zombies`), which has to know what a
    /// previous run left behind. An enumeration that cannot be made is an empty
    /// list: a sweep with nothing to look at must not stop a launch.
    fn list_processes(&self) -> Result<Vec<crate::zombies::RunningProcess>>;
}

/// Called when a service starts, stops, or fails to start.
pub type StateCallback = Arc<dyn Fn(bool, u32) + Send + Sync + 'static>;

/// What to run and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// The name shown in every log line and in the check for PostgreSQL.
    pub name: String,
    /// The executable to run.
    pub exe_path: PathBuf,
    /// Its arguments, in order.
    pub args: Vec<String>,
    /// The port it is expected to listen on. `0` means "do not check".
    pub port: u16,
    /// Its working directory, when it needs one.
    pub work_dir: Option<PathBuf>,
    /// Extra environment variables, over this process's own.
    pub env: Vec<(String, String)>,
}

impl ServiceConfig {
    /// A service that runs `exe_path`.
    pub fn new(name: impl Into<String>, exe_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            exe_path: exe_path.into(),
            args: Vec::new(),
            port: 0,
            work_dir: None,
            env: Vec::new(),
        }
    }

    /// Sets the arguments.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the port the service is expected to hold.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets the working directory.
    pub fn work_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(directory.into());
        self
    }

    /// Adds an environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Whether this is the PostgreSQL path.
    pub fn is_postgres(&self) -> bool {
        self.name == POSTGRES
    }

    /// The data directory from the arguments, when one was given.
    ///
    /// PostgreSQL is told where its cluster lives with `-D`, and both the
    /// `postmaster.pid` lookup and the `pg_ctl` stop are built from it.
    pub fn data_dir(&self) -> Option<PathBuf> {
        postgres_data_dir(&self.args)
    }

    /// `pg_ctl.exe`, which PostgreSQL ships next to its server.
    pub fn pg_ctl_path(&self) -> PathBuf {
        self.exe_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(PG_CTL_PROGRAM)
    }

    /// The process to start.
    fn spec(&self) -> ProcessSpec {
        let mut spec = ProcessSpec::new(&self.exe_path, &self.name).args(self.args.clone());
        if let Some(work_dir) = &self.work_dir {
            spec = spec.cwd(work_dir);
        }
        for (key, value) in &self.env {
            spec = spec.env(key, value);
        }
        spec
    }
}

/// The data directory a PostgreSQL command line names, if it names one.
pub fn postgres_data_dir(args: &[String]) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        if argument == "-D" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

/// The command line that starts PostgreSQL through `runas`.
///
/// `self_exe` is this executable, which is re-entered with `--hide-run` so the
/// server is started with a hidden window and without the interface waiting for
/// it. Without it - the original's fallback when `os.Executable()` failed - the
/// server itself is run, still through `runas`.
pub fn runas_command_line(self_exe: Option<&Path>, exe_path: &Path, data_dir: &Path) -> String {
    match self_exe {
        Some(self_exe) => format!(
            "\"{}\" {HIDE_RUN_FLAG} \"{}\" -D \"{}\"",
            self_exe.display(),
            exe_path.display(),
            data_dir.display()
        ),
        None => format!("\"{}\" -D \"{}\"", exe_path.display(), data_dir.display()),
    }
}

/// The `runas` invocation that starts that command line at a restricted level.
///
/// `runas` takes the command to run as one argument, which is why a single
/// string is built and passed as one: it is a command line, not a shell string.
pub fn runas_spec(command_line: &str, work_dir: Option<&Path>) -> ProcessSpec {
    let mut spec = ProcessSpec::new("runas", "runas")
        .arg("/trustlevel:0x20000")
        .arg(command_line);
    if let Some(work_dir) = work_dir {
        spec = spec.cwd(work_dir);
    }
    spec
}

/// Reads PostgreSQL's own PID out of `postmaster.pid`.
///
/// PostgreSQL writes the file a moment after it starts, so the original looked
/// for it ten times, 200 ms apart, and gave up with the last error. The PID is
/// on the first line, and nothing else in the file is of interest.
pub fn read_postmaster_pid(
    host: &dyn ServiceHost,
    data_dir: &Path,
) -> std::result::Result<u32, String> {
    let pid_file = data_dir.join(POSTMASTER_PID_FILE);
    let mut last_error = None;

    for _ in 0..PID_POLL_ATTEMPTS {
        match std::fs::read_to_string(&pid_file) {
            Ok(text) if !text.is_empty() => return parse_postmaster_pid(&text),
            Ok(_) => last_error = Some("empty postmaster.pid".to_owned()),
            // The original's message named the file it could not open
            // (`open <path>: ...`), and that name is the useful part.
            Err(error) => last_error = Some(format!("open {}: {error}", pid_file.display())),
        }
        host.sleep(PID_POLL_INTERVAL);
    }

    Err(last_error.unwrap_or_else(|| "empty postmaster.pid".to_owned()))
}

/// The PID on the first line of `postmaster.pid`.
fn parse_postmaster_pid(text: &str) -> std::result::Result<u32, String> {
    let first = text
        .split('\n')
        .next()
        .ok_or_else(|| "empty postmaster.pid".to_owned())?;
    first
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("invalid PID in postmaster.pid: {error}"))
}

/// The log file PostgreSQL says it is writing, from `current_logfiles`.
///
/// The file holds lines like `stderr log/postgresql-2026-01-01_000000.log`, and
/// the first one is the file the engine follows.
pub fn log_file_from_current_logfiles(data_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(data_dir.join(CURRENT_LOGFILES)).ok()?;
    if text.is_empty() {
        return None;
    }
    for line in text.split('\n') {
        let line = line.trim();
        if let Some(relative) = line.strip_prefix("stderr ") {
            return Some(data_dir.join(relative));
        }
    }
    None
}

/// The most recently written `.log` under `<data dir>\log`.
pub fn newest_log_file(data_dir: &Path) -> Option<PathBuf> {
    let log_dir = data_dir.join(LOG_SUBDIR);
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(log_dir).ok()?.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        // A directory is skipped before its name is looked at, which is what
        // the original's `entry.IsDir()` did: `directory.log` is not a log.
        if metadata.is_dir()
            || path
                .extension()
                .is_none_or(|extension| extension != LOG_EXTENSION)
        {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        candidates.push((path, modified));
    }

    // `os.ReadDir` hands the entries over sorted by name, and the original kept
    // the first file it found for a given timestamp, so the order is part of the
    // answer whenever two logs were written in the same instant.
    candidates.sort_by(|(left, _), (right, _)| left.file_name().cmp(&right.file_name()));

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for (path, modified) in candidates {
        if newest.as_ref().is_none_or(|(when, _)| modified > *when) {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Where PostgreSQL is logging, once it is there.
///
/// `current_logfiles` wins while it names an existing file; the newest `.log`
/// is the fallback. A candidate that is not there yet is retried, not replaced,
/// which is what the original's single variable did.
fn find_postgres_log(
    host: &dyn ServiceHost,
    stop: &AtomicBool,
    data_dir: &Path,
) -> Option<PathBuf> {
    let mut log_path: Option<PathBuf> = None;
    for _ in 0..LOG_LOOKUP_ATTEMPTS {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        if log_path.is_none() {
            log_path = log_file_from_current_logfiles(data_dir);
        }
        if log_path.is_none() {
            log_path = newest_log_file(data_dir);
        }
        if let Some(path) = &log_path
            && path.exists()
        {
            break;
        }
        host.sleep(LOG_LOOKUP_INTERVAL);
    }
    log_path
}

/// Reads `path` as it grows, handing every complete line to `sink`.
///
/// A line that has not ended yet is held back, which is what the original's
/// rune-by-rune reader did: a half-written log entry must not appear as one.
/// A read error ends the follow, as it did there.
pub fn follow_log(
    path: &Path,
    stop: &AtomicBool,
    sink: &dyn Fn(&str),
    sleep: &dyn Fn(Duration),
) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut pending: Vec<u8> = Vec::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(_) => return Ok(()),
        };
        if available.is_empty() {
            sleep(LOG_POLL_INTERVAL);
            continue;
        }

        match available.iter().rposition(|byte| *byte == b'\n') {
            None => {
                let take = available.len();
                pending.extend_from_slice(available);
                reader.consume(take);
            }
            Some(last_newline) => {
                pending.extend_from_slice(&available[..=last_newline]);
                reader.consume(last_newline + 1);
                for line in pending.split(|byte| *byte == b'\n') {
                    emit_log_line(line, sink);
                }
                pending.clear();
            }
        }
    }
}

/// Hands one log line over, unless it is empty once the line ending is gone.
fn emit_log_line(raw: &[u8], sink: &dyn Fn(&str)) {
    let text = String::from_utf8_lossy(raw);
    let line = text.trim_end_matches('\r');
    if !line.is_empty() {
        sink(line);
    }
}

/// PostgreSQL's log line, reduced to its level and message.
///
/// PostgreSQL writes `<timestamp> <zone> [<pid>] LOG:  message` - note the two
/// spaces after the level - and the original showed only `LOG: message`.
/// Anything else is passed through unchanged.
pub fn postgres_log_line(line: &str) -> String {
    if let Some(index) = line.find(":  ") {
        let prefix = &line[..index];
        let message = &line[index + 3..];
        if let Some(level) = prefix.split_whitespace().next_back() {
            return format!("{level}: {message}");
        }
    }
    line.to_owned()
}

/// Streams a pipe into a line sink until it ends.
///
/// Used for a service's own output: the original scanned its pipes the same way
/// and dropped empty lines. The scanner's buffer ceiling is not reproduced -
/// reading a pipe in chunks has no line length to exceed.
pub fn stream_to_log(mut reader: impl Read, sink: &dyn Fn(&str)) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                pending.extend_from_slice(&chunk[..read]);
                let mut start = 0;
                while let Some(position) = pending[start..].iter().position(|byte| *byte == b'\n') {
                    emit_log_line(&pending[start..start + position], sink);
                    start += position + 1;
                }
                pending.drain(..start);
            }
            Err(_) => break,
        }
    }
    // A stream can end without a final line ending, which the original's
    // scanner also reported.
    if !pending.is_empty() {
        emit_log_line(&pending, sink);
    }
}

/// What a [`Service`] is holding on to.
#[derive(Default)]
struct Held {
    process: Option<Arc<dyn HostedProcess>>,
    /// Set when the PostgreSQL log follower should stop.
    tail_stop: Option<Arc<AtomicBool>>,
}

/// One supervised service.
///
/// Built once and shared: the constructor returns an `Arc` because the engine
/// keeps the service alive in the threads that stream its output and wait for
/// it to exit.
pub struct Service {
    host: Arc<dyn ServiceHost>,
    config: ServiceConfig,
    log: LogFn,
    state_callback: Mutex<Option<StateCallback>>,
    held: Mutex<Held>,
    /// Serializes starts, so two interfaces cannot start the same service at
    /// once and produce two servers.
    starting: Mutex<()>,
}

impl Service {
    /// A service over `config`, logging through `log`.
    ///
    /// The original set its logger and its state callback after construction;
    /// this takes the logger up front, because a shared handle has nowhere
    /// sensible to put a late one.
    pub fn new(host: Arc<dyn ServiceHost>, config: ServiceConfig, log: LogFn) -> Arc<Self> {
        Arc::new(Self {
            host,
            config,
            log,
            state_callback: Mutex::new(None),
            held: Mutex::new(Held::default()),
            starting: Mutex::new(()),
        })
    }

    /// A service over `config` that says nothing, for callers that only need it
    /// to run.
    pub fn quiet(host: Arc<dyn ServiceHost>, config: ServiceConfig) -> Arc<Self> {
        Self::new(host, config, nop_log())
    }

    /// Sets the callback told whenever the service starts, stops, or fails.
    pub fn set_state_callback(&self, callback: StateCallback) {
        *self.state_callback.lock().unwrap_or_else(poisoned) = Some(callback);
    }

    /// What this service runs.
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    /// Its name, as every log line spells it.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Whether this engine is holding a process for it, or a process of this
    /// service is running that this engine did not start.
    ///
    /// The original's `Running()`: true as soon as a start has recorded a
    /// process, and false once it has exited. It is not a probe of the
    /// operating system - a service that died without the wait loop noticing
    /// still reads as running for the moment it takes to notice.
    ///
    /// What is added is the cross-process case, and it is what makes the
    /// command line work at all: `lambo up` and the `lambo status` that follows
    /// it are two processes, and the second one has no handle on what the first
    /// started. [`Service::observed_pid`] recognises Lambo's own program
    /// running for this service, so "already running", "running" and "stop it"
    /// are answered truthfully from another process - and a service this engine
    /// holds is still answered from that handle, without consulting the
    /// operating system at all.
    pub fn running(&self) -> bool {
        self.pid().is_some()
    }

    /// Its process id, from the handle when this engine holds one and from the
    /// operating system when it does not.
    pub fn pid(&self) -> Option<u32> {
        if let Some(process) = self.held_process() {
            return Some(process.pid());
        }
        self.observed_pid()
    }

    /// The process running this service's own executable, when there is one.
    ///
    /// The match is by executable path, which is the same rule the start-up
    /// sweep uses to recognise Lambo's programs (`<base>/bin/...` and the
    /// managed runtimes) and the only evidence a process does not have to
    /// volunteer. When several processes run it - a leftover from a crashed run
    /// next to a healthy one, which is exactly what the sweep exists for - the
    /// one holding the service's port is the service; a service without a port
    /// takes the only match it has.
    fn observed_pid(&self) -> Option<u32> {
        if self.config.exe_path.as_os_str().is_empty() {
            return None;
        }
        let candidates: Vec<u32> = self
            .host
            .list_processes()
            .unwrap_or_default()
            .into_iter()
            .filter(|process| same_executable(&process.path, &self.config.exe_path))
            .map(|process| process.pid)
            .collect();

        match candidates.as_slice() {
            [] => None,
            [only] => Some(*only),
            _ => {
                let owner =
                    crate::port::listening_pid(self.config.port, crate::platform::Os::host())?;
                candidates.into_iter().find(|pid| *pid == owner)
            }
        }
    }

    /// The process id of the process this engine is holding, when it holds one.
    ///
    /// The narrow answer, for callers that must not touch a process they did not
    /// start. The start-up sweep is the one that matters: it keeps exactly the
    /// services *this* engine runs and kills everything else of this
    /// installation's under `bin/`, so a leftover from a crashed run - the same
    /// program, started by a process that is gone - is still swept. Asking
    /// [`pid`](Self::pid) there would keep the leftover, because a process of
    /// that program is what it reports.
    pub fn held_pid(&self) -> Option<u32> {
        self.held_process().map(|process| process.pid())
    }

    /// The process this engine is holding, when it holds one.
    fn held_process(&self) -> Option<Arc<dyn HostedProcess>> {
        self.held.lock().unwrap_or_else(poisoned).process.clone()
    }

    /// Starts the service.
    ///
    /// Fails without starting anything when it is already running or when its
    /// port is taken, which is what the original reported and what the card in
    /// the interface shows.
    pub fn start(self: &Arc<Self>) -> Result<()> {
        let _starting = self.starting.lock().unwrap_or_else(poisoned);

        if let Some(pid) = self.running_pid_for_start() {
            return Err(self.failure(format!("{} already running (pid {pid})", self.config.name)));
        }
        if self.config.port > 0 && self.host.port_busy(self.config.port) {
            return Err(self.failure(format!("port {} already in use", self.config.port)));
        }

        if self.config.is_postgres() {
            return self.start_postgres();
        }
        self.start_plain()
    }

    /// The PID of the process holding this service's port, when one of its own
    /// programs does.
    ///
    /// The strict answer, for callers that cannot be misled by a second copy of
    /// the same program: one PHP runtime serves every project, so "a process
    /// running `php`" is not evidence that *this* project's server is up - the
    /// port is. A service with no port has no port to ask about, and falls back
    /// to the process of its program.
    pub fn pid_holding_port(&self) -> Option<u32> {
        if let Some(process) = self.held_process() {
            return Some(process.pid());
        }
        self.running_pid_for_start()
    }

    /// The PID that makes a start refuse because the service is already up.
    ///
    /// A handle this engine holds is decisive. From another process, only the
    /// process that holds *this service's port* is: a second copy of the same
    /// program elsewhere - a leftover from a crashed run, or another project's
    /// server - is a different instance, and refusing to start because of it
    /// would leave a user with a server that never comes up. A service with no
    /// port has no such evidence to weigh, so there a process of its own
    /// program is the answer.
    fn running_pid_for_start(&self) -> Option<u32> {
        if let Some(process) = self.held_process() {
            return Some(process.pid());
        }
        if self.config.port == 0 {
            return self.observed_pid();
        }
        let owner = crate::port::listening_pid(self.config.port, crate::platform::Os::host())?;
        self.host
            .list_processes()
            .unwrap_or_default()
            .into_iter()
            .find(|process| {
                process.pid == owner && same_executable(&process.path, &self.config.exe_path)
            })
            .map(|process| process.pid)
    }

    /// Starts an ordinary service and starts watching it.
    fn start_plain(self: &Arc<Self>) -> Result<()> {
        let spec = self.config.spec();
        let process = self.host.start(&spec, true)?;
        let pid = process.pid();

        self.hold(Arc::clone(&process), None);
        self.note(&format!("started (pid {pid})"));
        self.report_state(true, pid);

        if let Some(stdout) = process.take_stdout() {
            self.stream(stdout);
        }
        if let Some(stderr) = process.take_stderr() {
            self.stream(stderr);
        }
        self.watch(process, None);

        Ok(())
    }

    /// Starts PostgreSQL the way the original had to.
    fn start_postgres(self: &Arc<Self>) -> Result<()> {
        let Some(data_dir) = self.config.data_dir() else {
            return Err(self.failure(format!(
                "{POSTGRES} data directory (-D) not found in arguments"
            )));
        };

        // The placeholder points at this process until the postmaster's own PID
        // is known; see the module documentation.
        let placeholder = self.host.process_handle(std::process::id());
        self.hold(Arc::clone(&placeholder), None);

        let pid_file = data_dir.join(POSTMASTER_PID_FILE);
        let _ = std::fs::remove_file(&pid_file);

        let this = Arc::clone(self);
        std::thread::spawn(move || this.run_postgres(data_dir, placeholder));
        Ok(())
    }

    /// The PostgreSQL start, off the caller's thread.
    ///
    /// The original returned as soon as the intermediary was spawned, because
    /// the postmaster takes seconds to write its PID file and the interface
    /// must not freeze while it does.
    fn run_postgres(self: Arc<Self>, data_dir: PathBuf, placeholder: Arc<dyn HostedProcess>) {
        self.note("launching postgres.exe directly in the background...");

        let command_line = runas_command_line(
            self.host.self_exe().ok().as_deref(),
            &self.config.exe_path,
            &data_dir,
        );
        let spec = runas_spec(&command_line, self.config.work_dir.as_deref());

        if let Err(error) = self.host.start(&spec, false) {
            self.note(&format!("failed to spawn runas: {error}"));
            if self.release(&placeholder) {
                self.report_state(false, 0);
            }
            return;
        }

        let pid = match read_postmaster_pid(self.host.as_ref(), &data_dir) {
            Ok(pid) => pid,
            Err(error) => {
                self.note(&format!("failed to read postmaster PID: {error}"));
                if self.release(&placeholder) {
                    self.report_state(false, 0);
                }
                return;
            }
        };

        // The service may have been replaced while the postmaster was starting,
        // in which case the placeholder is gone and this start is abandoned.
        let postmaster = self.host.process_handle(pid);
        if !self.adopt(&placeholder, Arc::clone(&postmaster)) {
            return;
        }

        self.note(&format!("started (pid {pid})"));
        self.report_state(true, pid);

        let tail_stop = Arc::new(AtomicBool::new(false));
        self.set_tail_stop(Arc::clone(&tail_stop));
        self.follow_postgres_log(data_dir, Arc::clone(&tail_stop));

        // The wait is on the *adopted* handle, which is what the original did:
        // it spawned this goroutine after `os.FindProcess(pid)` had replaced the
        // placeholder's process, so it observed the postmaster's exit and not
        // its own.
        self.watch(postmaster, Some(tail_stop));
    }

    /// Finds PostgreSQL's log and follows it until `stop` says otherwise.
    fn follow_postgres_log(self: &Arc<Self>, data_dir: PathBuf, stop: Arc<AtomicBool>) {
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let Some(path) = find_postgres_log(this.host.as_ref(), &stop, &data_dir) else {
                // The stop flag short-circuits the search, and the original said
                // nothing in that case either.
                if !stop.load(Ordering::SeqCst) {
                    this.note("warning: could not locate PostgreSQL log file");
                }
                return;
            };

            let sink = {
                let this = Arc::clone(&this);
                move |line: &str| this.note(&postgres_log_line(line))
            };
            let sleep = |duration: Duration| this.host.sleep(duration);

            if let Err(error) = follow_log(&path, &stop, &sink, &sleep) {
                // A follower that was stopped mid-read is not a failure to
                // report; one that could not open the file is.
                if !stop.load(Ordering::SeqCst) {
                    this.note(&format!(
                        "warning: failed to open PostgreSQL log file: {error}"
                    ));
                }
            }
        });
    }

    /// Stops the service.
    ///
    /// PostgreSQL is asked to stop through its own tool first; everything else
    /// goes straight to the tree kill. Either way the fallback is the same: the
    /// whole process tree, then the bare process - and only if both refuse is
    /// this an error.
    ///
    /// A service this engine is not holding is still stopped when one of its
    /// processes is running: the command line's `lambo down` is a different
    /// process from the `lambo up` that started the service, and "nothing is
    /// held" must not mean "nothing to stop".
    pub fn stop(&self) -> Result<()> {
        let Some(pid) = self.pid() else {
            return Ok(());
        };

        if self.config.is_postgres() && self.stop_postgres(pid) {
            return Ok(());
        }

        self.note(&format!("stopping (pid {pid})..."));
        let Err(taskkill) = self.host.kill_tree(pid) else {
            return Ok(());
        };
        match self.host.kill_process(pid) {
            Ok(()) => Ok(()),
            Err(kill) => Err(self.failure(format!(
                "kill {}: taskkill={taskkill}, proc.Kill={kill}",
                self.config.name
            ))),
        }
    }

    /// `pg_ctl stop -m fast`, which is how PostgreSQL wants to be asked.
    ///
    /// Returns whether the server was stopped: a `pg_ctl` that failed - or was
    /// never tried, because the arguments named no data directory - leaves the
    /// caller to fall through to the tree kill, and its output is logged so the
    /// reason is visible.
    fn stop_postgres(&self, pid: u32) -> bool {
        self.note(&format!("stopping (pid {pid}) via pg_ctl..."));
        let Some(data_dir) = self.config.data_dir() else {
            return false;
        };

        let spec = ProcessSpec::new(self.config.pg_ctl_path(), "pg_ctl").args([
            "stop",
            "-D",
            &data_dir.display().to_string(),
            "-m",
            "fast",
        ]);
        match self.host.run_captured(&spec) {
            Ok(run) if run.success => true,
            Ok(run) => {
                self.note(&format!(
                    "pg_ctl stop failed: {} (output: {:?})",
                    run.exit_reason(),
                    run.combined
                ));
                false
            }
            Err(error) => {
                self.note(&format!("pg_ctl stop failed: {error} (output: \"\")"));
                false
            }
        }
    }

    /// One line, prefixed the way every line this service writes is.
    fn note(&self, line: &str) {
        (self.log)(&format!("[{}] {line}", self.config.name));
    }

    /// Tells the interface what happened.
    fn report_state(&self, running: bool, pid: u32) {
        let callback = self.state_callback.lock().unwrap_or_else(poisoned).clone();
        if let Some(callback) = callback {
            callback(running, pid);
        }
    }

    /// Records the process this service is running.
    fn hold(&self, process: Arc<dyn HostedProcess>, tail_stop: Option<Arc<AtomicBool>>) {
        let mut held = self.held.lock().unwrap_or_else(poisoned);
        held.process = Some(process);
        held.tail_stop = tail_stop;
    }

    /// Points the running service at the postmaster, unless it has been stopped
    /// since - which is the original's `if s.cmd != placeholderCmd` check.
    fn adopt(
        &self,
        placeholder: &Arc<dyn HostedProcess>,
        postmaster: Arc<dyn HostedProcess>,
    ) -> bool {
        let mut held = self.held.lock().unwrap_or_else(poisoned);
        if held
            .process
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, placeholder))
        {
            held.process = Some(postmaster);
            return true;
        }
        false
    }

    /// Lets go of the placeholder, reporting whether it was still ours.
    fn release(&self, placeholder: &Arc<dyn HostedProcess>) -> bool {
        let mut held = self.held.lock().unwrap_or_else(poisoned);
        let ours = held
            .process
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, placeholder));
        if ours {
            held.process = None;
        }
        ours
    }

    /// Records the follower's stop flag.
    fn set_tail_stop(&self, stop: Arc<AtomicBool>) {
        self.held.lock().unwrap_or_else(poisoned).tail_stop = Some(stop);
    }

    /// Streams one of the service's pipes into the log.
    fn stream(self: &Arc<Self>, reader: Box<dyn Read + Send>) {
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let sink = |line: &str| this.note(line);
            stream_to_log(reader, &sink);
        });
    }

    /// Waits for the service to end, then says so and lets go of it.
    ///
    /// `tail_stop` is the PostgreSQL log follower's flag: it is closed before
    /// the exit is reported, so nothing is written to the log after the server
    /// has gone.
    fn watch(
        self: &Arc<Self>,
        process: Arc<dyn HostedProcess>,
        tail_stop: Option<Arc<AtomicBool>>,
    ) {
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            let outcome = process.wait();
            if let Some(flag) = &tail_stop {
                flag.store(true, Ordering::SeqCst);
            }
            let cleared = {
                let mut held = this.held.lock().unwrap_or_else(poisoned);
                let ours = held
                    .process
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &process));
                if ours {
                    held.process = None;
                    held.tail_stop = None;
                }
                ours
            };

            match outcome {
                WaitOutcome::Clean => this.note("exited cleanly"),
                WaitOutcome::Failed(reason) => this.note(&format!("exited: {reason}")),
            }
            if cleared {
                this.report_state(false, 0);
            }
        });
    }

    /// An error in the original's own words.
    fn failure(&self, reason: impl Into<String>) -> Error {
        Error::InvalidInput(reason.into())
    }
}

/// The `--hide-run` short circuit.
///
/// `lambo --hide-run <program> [args…]` runs the program with a hidden window
/// and exits at once, which is how the PostgreSQL launcher detaches the server
/// from the interface. Returns `None` when the flag is not there, so callers
/// carry on with whatever they were doing.
///
/// The program is started, never waited for: the caller is a short-lived
/// intermediary and the child outlives it.
pub fn hide_run(argv: &[String], host: &dyn ServiceHost) -> Option<Result<()>> {
    if argv.len() < 2 || argv[0] != HIDE_RUN_FLAG {
        return None;
    }

    let mut spec = ProcessSpec::new(&argv[1], "hide-run")
        .args(argv[2..].to_vec())
        .detached();
    // The original inherited its handles here, which for the windowed interface
    // means none at all; a detached child is given null handles instead, so
    // nothing of the interface's can leak into the server.
    spec.stdin = crate::process::Output::Null;
    spec.stdout = crate::process::Output::Null;
    spec.stderr = crate::process::Output::Null;

    Some(host.start(&spec, false).map(|_| ()))
}

/// The `--hide-run` short circuit for an entry point.
///
/// `argv` is the arguments *after* the program's own name, which is the shape
/// [`hide_run`] expects. `None` means this is an ordinary command line and the
/// interface should carry on.
///
/// A failed start is returned so the caller can say something about it, but the
/// original's exit status did not depend on it: the launcher reads the PID file
/// and reports the failure from there.
pub fn hide_run_from_argv(argv: &[String]) -> Option<Result<()>> {
    let host = HostService::new();
    hide_run(argv, &host)
}

/// The real host: [`crate::process`] and the operating system.
pub struct HostService {
    os: crate::platform::Os,
    /// The process table, kept briefly.
    ///
    /// Enumerating processes is not free - on Windows it is a PowerShell
    /// launch - and the engine asks about one service at a time: a `status`
    /// over thirty services would otherwise be thirty launches for the same
    /// answer. The table is a snapshot of a moment, so it is held for
    /// [`PROCESS_TABLE_TTL`] and taken again after that; anything this engine
    /// started is known by its own handle and never needs the table at all.
    table: Mutex<Option<(std::time::Instant, Arc<Vec<crate::zombies::RunningProcess>>)>>,
}

/// How long [`HostService`] keeps a process table before taking a fresh one.
///
/// Below every poll interval the engine has (the interface refreshes at 500 ms,
/// the lifecycle waits poll at 50 ms) and far below any timeout, so a process
/// that has exited is reported as gone within one poll of the next frame.
pub const PROCESS_TABLE_TTL: Duration = Duration::from_millis(200);

impl HostService {
    /// The host for this machine.
    pub fn new() -> Self {
        Self {
            os: crate::platform::Os::host(),
            table: Mutex::new(None),
        }
    }

    /// The platform this host was built for.
    pub fn os(&self) -> crate::platform::Os {
        self.os
    }

    /// The process table, from the cache when it is fresh enough.
    fn process_table(&self) -> Vec<crate::zombies::RunningProcess> {
        let mut cache = self.table.lock().unwrap_or_else(poisoned);
        if let Some((taken, table)) = cache.as_ref() {
            if taken.elapsed() < PROCESS_TABLE_TTL {
                return table.as_ref().clone();
            }
        }

        let table = Arc::new(self.enumerate_processes());
        *cache = Some((std::time::Instant::now(), Arc::clone(&table)));
        table.as_ref().clone()
    }

    /// Asks the platform for every process that has a path.
    fn enumerate_processes(&self) -> Vec<crate::zombies::RunningProcess> {
        if !self.os.is_windows() {
            return crate::zombies::proc_processes();
        }
        // The original ran this pipeline and ignored a failure; an empty table
        // is the same outcome, and the sweep simply finds nothing to kill.
        match self.run_captured(&crate::zombies::process_list_spec()) {
            Ok(run) => crate::zombies::parse_process_table(&run.stdout),
            Err(_) => Vec::new(),
        }
    }
}

impl Default for HostService {
    fn default() -> Self {
        Self::new()
    }
}

/// A process the engine started through [`HostService`].
struct HostedChild {
    child: std::sync::Mutex<Option<crate::process::Child>>,
    stdout: Mutex<Option<Box<dyn Read + Send>>>,
    stderr: Mutex<Option<Box<dyn Read + Send>>>,
    pid: u32,
}

impl HostedProcess for HostedChild {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn wait(&self) -> WaitOutcome {
        let mut guard = self.child.lock().unwrap_or_else(poisoned);
        let Some(child) = guard.as_mut() else {
            return WaitOutcome::Clean;
        };
        match child.wait() {
            Ok(status) if status.success() => WaitOutcome::Clean,
            Ok(status) => WaitOutcome::Failed(exit_status_text(status)),
            Err(error) => WaitOutcome::Failed(error.to_string()),
        }
    }

    fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
        let mut stdout = self.stdout.lock().unwrap_or_else(poisoned);
        stdout.take()
    }

    fn take_stderr(&self) -> Option<Box<dyn Read + Send>> {
        let mut stderr = self.stderr.lock().unwrap_or_else(poisoned);
        stderr.take()
    }
}

/// A process handle the engine only knows by PID.
///
/// Used for the PostgreSQL placeholder and for the postmaster itself, where the
/// intermediary's exit says nothing about the server.
struct PidHandle {
    pid: u32,
    os: crate::platform::Os,
}

impl HostedProcess for PidHandle {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn wait(&self) -> WaitOutcome {
        // Polling rather than `WaitForSingleObject`, because the handle may be
        // one the engine never owned: `OpenProcess` on another user's process
        // needs a right a restricted token does not hand out, and the original
        // ignored that failure and reported the wait error instead.
        let mut waited = Duration::ZERO;
        while waited < PID_WAIT_CEILING {
            if !crate::process::is_running(self.pid, self.os) {
                return WaitOutcome::Clean;
            }
            std::thread::sleep(PID_POLL_INTERVAL);
            waited += PID_POLL_INTERVAL;
        }
        WaitOutcome::Clean
    }

    fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
        None
    }

    fn take_stderr(&self) -> Option<Box<dyn Read + Send>> {
        None
    }
}

/// How long a PID-only wait polls before giving up.
///
/// Postgres' server runs for as long as the user wants it to, so this ceiling
/// is a backstop against a PID that can never be observed to end rather than a
/// deadline: reaching it reports a clean exit, which is what the state callback
/// would have said when the interface itself went away.
const PID_WAIT_CEILING: Duration = Duration::from_secs(60 * 60 * 24 * 365);

impl ServiceHost for HostService {
    fn port_busy(&self, port: u16) -> bool {
        // The original bound the port and treated a failure as "busy", which is
        // what the fallback was protecting against; this asks the platform the
        // same question and, on a port that is merely slow to release, waits
        // the same moment the original did.
        let busy = !crate::port::is_free(port);
        self.sleep(PORT_PROBE_PAUSE);
        busy
    }

    fn start(&self, spec: &ProcessSpec, piped: bool) -> Result<Arc<dyn HostedProcess>> {
        if piped {
            let child = crate::process::spawn_service(spec, self.os)?;
            let pid = child.pid;
            return Ok(Arc::new(HostedChild {
                child: std::sync::Mutex::new(Some(child.child)),
                stdout: Mutex::new(Some(child.stdout)),
                stderr: Mutex::new(Some(child.stderr)),
                pid,
            }));
        }

        let child = crate::process::spawn(spec, self.os)?;
        let pid = child.id();
        // The child keeps running: the engine supervises it by PID and by the
        // proxy handle, not by holding its exit status.
        drop(child);
        Ok(Arc::new(PidHandle { pid, os: self.os }))
    }

    fn process_handle(&self, pid: u32) -> Arc<dyn HostedProcess> {
        Arc::new(PidHandle { pid, os: self.os })
    }

    fn kill_tree(&self, pid: u32) -> Result<()> {
        crate::process::terminate_tree(pid, self.os, true)
    }

    fn kill_process(&self, pid: u32) -> Result<()> {
        crate::process::kill_process_now(pid, self.os)
    }

    fn run_captured(&self, spec: &ProcessSpec) -> Result<CapturedRun> {
        let output = crate::process::run(spec, self.os)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Ok(CapturedRun {
            success: output.status.success(),
            code: output.status.code(),
            combined: format!("{stdout}{stderr}"),
            stdout,
            stderr,
        })
    }

    fn self_exe(&self) -> Result<PathBuf> {
        std::env::current_exe()
            .map_err(|source| Error::io("the executable running this program", source))
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn list_processes(&self) -> Result<Vec<crate::zombies::RunningProcess>> {
        Ok(self.process_table())
    }
}

/// Whether two paths name the same program.
///
/// Compared as text, because that is what the platform's own process table
/// reports: Windows writes the path it has, in whatever case it was created
/// with, so the comparison is case-insensitive there and exact on Unix, where
/// two spellings are two files.
fn same_executable(candidate: &Path, expected: &Path) -> bool {
    if candidate == expected {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    candidate
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
}

/// An exit status in the words the original's `%v` produced.
fn exit_status_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {code}"),
        None => status.to_string(),
    }
}

/// A lock taken by a thread that panicked while holding it.
///
/// The engine's locks guard two options and a flag; nothing behind them is left
/// half-written by a panic, and refusing to run the rest of the program because
/// one thread died would be worse than carrying on.
fn poisoned<T>(error: std::sync::PoisonError<T>) -> T {
    error.into_inner()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Cursor, Read};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use super::*;
    use crate::testutil::TempDir;

    /// A latch a test opens to let a scripted process end.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        signal: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(poisoned);
            while !*open {
                open = self.signal.wait(open).unwrap_or_else(poisoned);
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(poisoned) = true;
            self.signal.notify_all();
        }
    }

    /// A process whose ending a test controls.
    struct ScriptedProcess {
        pid: u32,
        outcome: WaitOutcome,
        gate: Option<Arc<Gate>>,
        stdout: Mutex<Option<Box<dyn Read + Send>>>,
        stderr: Mutex<Option<Box<dyn Read + Send>>>,
    }

    impl ScriptedProcess {
        fn new(pid: u32, outcome: WaitOutcome) -> Arc<Self> {
            Arc::new(Self {
                pid,
                outcome,
                gate: None,
                stdout: Mutex::new(None),
                stderr: Mutex::new(None),
            })
        }
    }

    impl HostedProcess for ScriptedProcess {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn wait(&self) -> WaitOutcome {
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            self.outcome.clone()
        }

        fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
            self.stdout.lock().unwrap_or_else(poisoned).take()
        }

        fn take_stderr(&self) -> Option<Box<dyn Read + Send>> {
            self.stderr.lock().unwrap_or_else(poisoned).take()
        }
    }

    /// What the next `start` should do.
    #[derive(Clone)]
    enum Reply {
        /// A process that starts and behaves as described.
        Alive {
            pid: u32,
            stdout: String,
            stderr: String,
            /// Held back until the test releases it, when given.
            gate: Option<Arc<Gate>>,
            /// What it reports when it ends.
            outcome: WaitOutcome,
        },
        /// A start the platform refuses.
        Refuses(String),
    }

    impl Reply {
        fn alive(pid: u32) -> Self {
            Self::Alive {
                pid,
                stdout: String::new(),
                stderr: String::new(),
                gate: None,
                outcome: WaitOutcome::Clean,
            }
        }

        fn held(self, gate: Arc<Gate>) -> Self {
            match self {
                Self::Alive {
                    pid,
                    stdout,
                    stderr,
                    outcome,
                    ..
                } => Self::Alive {
                    pid,
                    stdout,
                    stderr,
                    gate: Some(gate),
                    outcome,
                },
                other => other,
            }
        }

        fn speaking(self, stdout: &str, stderr: &str) -> Self {
            match self {
                Self::Alive {
                    pid, gate, outcome, ..
                } => Self::Alive {
                    pid,
                    stdout: stdout.to_owned(),
                    stderr: stderr.to_owned(),
                    gate,
                    outcome,
                },
                other => other,
            }
        }

        fn failing_with(self, outcome: WaitOutcome) -> Self {
            match self {
                Self::Alive {
                    pid,
                    stdout,
                    stderr,
                    gate,
                    ..
                } => Self::Alive {
                    pid,
                    stdout,
                    stderr,
                    gate,
                    outcome,
                },
                other => other,
            }
        }
    }

    /// What a captured run should return.
    #[derive(Clone)]
    enum Capture {
        Run(CapturedRun),
        Refuses(String),
    }

    /// One recorded start.
    struct Started {
        spec: ProcessSpec,
        piped: bool,
    }

    /// The platform, scripted.
    #[derive(Default)]
    struct ScriptedHost {
        busy_ports: Vec<u16>,
        replies: Mutex<VecDeque<Reply>>,
        default_reply: Mutex<Option<Reply>>,
        starts: Mutex<Vec<Started>>,
        killed: Mutex<Vec<u32>>,
        killed_one: Mutex<Vec<u32>>,
        tree_kill_fails: bool,
        one_kill_fails: bool,
        captured: Mutex<Vec<ProcessSpec>>,
        capture: Mutex<Option<Capture>>,
        self_exe: Mutex<Option<PathBuf>>,
        no_self_exe: bool,
        handles: Mutex<Vec<u32>>,
        /// Released when a PID-addressed process is meant to be over.
        handles_gate: Arc<Gate>,
        sleep_scale: Duration,
        sleep_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    }

    impl ScriptedHost {
        fn with_busy_port(port: u16) -> Self {
            Self {
                busy_ports: vec![port],
                ..Self::default()
            }
        }

        fn replying(self, reply: Reply) -> Self {
            *self.default_reply.lock().unwrap_or_else(poisoned) = Some(reply);
            self
        }

        fn capturing(self, capture: Capture) -> Self {
            *self.capture.lock().unwrap_or_else(poisoned) = Some(capture);
            self
        }

        fn refusing_tree_kill(self) -> Self {
            Self {
                tree_kill_fails: true,
                ..self
            }
        }

        fn refusing_every_kill(self) -> Self {
            Self {
                tree_kill_fails: true,
                one_kill_fails: true,
                ..self
            }
        }

        fn without_self_exe(self) -> Self {
            Self {
                no_self_exe: true,
                ..self
            }
        }

        /// Makes every wait long enough for a test to act on.
        fn slow(self) -> Self {
            Self {
                sleep_scale: Duration::from_millis(60),
                ..self
            }
        }

        fn on_sleep(&self, hook: Arc<dyn Fn() + Send + Sync>) {
            *self.sleep_hook.lock().unwrap_or_else(poisoned) = Some(hook);
        }

        fn started(&self) -> Vec<(String, Vec<String>, bool)> {
            self.starts
                .lock()
                .unwrap_or_else(poisoned)
                .iter()
                .map(|started| {
                    (
                        started.spec.program.display().to_string(),
                        started.spec.args.clone(),
                        started.piped,
                    )
                })
                .collect()
        }

        fn started_specs(&self) -> Vec<ProcessSpec> {
            self.starts
                .lock()
                .unwrap_or_else(poisoned)
                .iter()
                .map(|started| started.spec.clone())
                .collect()
        }

        fn handled_pids(&self) -> Vec<u32> {
            self.handles.lock().unwrap_or_else(poisoned).clone()
        }

        fn killed(&self) -> Vec<u32> {
            self.killed.lock().unwrap_or_else(poisoned).clone()
        }

        fn killed_one(&self) -> Vec<u32> {
            self.killed_one.lock().unwrap_or_else(poisoned).clone()
        }

        fn capture_specs(&self) -> Vec<ProcessSpec> {
            self.captured.lock().unwrap_or_else(poisoned).clone()
        }
    }

    impl ServiceHost for ScriptedHost {
        fn port_busy(&self, port: u16) -> bool {
            self.busy_ports.contains(&port)
        }

        fn start(&self, spec: &ProcessSpec, piped: bool) -> Result<Arc<dyn HostedProcess>> {
            self.starts.lock().unwrap_or_else(poisoned).push(Started {
                spec: spec.clone(),
                piped,
            });

            let reply = self
                .replies
                .lock()
                .unwrap_or_else(poisoned)
                .pop_front()
                .or_else(|| self.default_reply.lock().unwrap_or_else(poisoned).clone())
                .unwrap_or_else(|| Reply::alive(4242));

            match reply {
                Reply::Refuses(reason) => Err(Error::InvalidInput(reason)),
                Reply::Alive {
                    pid,
                    stdout,
                    stderr,
                    gate,
                    outcome,
                } => {
                    let process = Arc::new(ScriptedProcess {
                        pid,
                        outcome,
                        gate,
                        stdout: Mutex::new(Some(Box::new(Cursor::new(stdout.into_bytes())))),
                        stderr: Mutex::new(Some(Box::new(Cursor::new(stderr.into_bytes())))),
                    });
                    Ok(process)
                }
            }
        }

        fn process_handle(&self, pid: u32) -> Arc<dyn HostedProcess> {
            self.handles.lock().unwrap_or_else(poisoned).push(pid);
            // Waiting on a PID that is alive blocks, as it does on Windows; the
            // test opens `handles_gate` when the process is meant to be gone.
            Arc::new(ScriptedProcess {
                pid,
                outcome: WaitOutcome::Clean,
                gate: Some(Arc::clone(&self.handles_gate)),
                stdout: Mutex::new(None),
                stderr: Mutex::new(None),
            })
        }

        fn kill_tree(&self, pid: u32) -> Result<()> {
            if self.tree_kill_fails {
                return Err(Error::InvalidInput("taskkill: access denied".to_owned()));
            }
            self.killed.lock().unwrap_or_else(poisoned).push(pid);
            Ok(())
        }

        fn kill_process(&self, pid: u32) -> Result<()> {
            if self.one_kill_fails {
                return Err(Error::InvalidInput("proc.Kill: access denied".to_owned()));
            }
            self.killed_one.lock().unwrap_or_else(poisoned).push(pid);
            Ok(())
        }

        fn run_captured(&self, spec: &ProcessSpec) -> Result<CapturedRun> {
            self.captured
                .lock()
                .unwrap_or_else(poisoned)
                .push(spec.clone());
            match self
                .capture
                .lock()
                .unwrap_or_else(poisoned)
                .clone()
                .unwrap_or(Capture::Run(CapturedRun {
                    success: true,
                    code: Some(0),
                    ..CapturedRun::default()
                })) {
                Capture::Run(run) => Ok(run),
                Capture::Refuses(reason) => Err(Error::InvalidInput(reason)),
            }
        }

        fn self_exe(&self) -> Result<PathBuf> {
            if self.no_self_exe {
                return Err(Error::InvalidInput("no executable path".to_owned()));
            }
            Ok(self
                .self_exe
                .lock()
                .unwrap_or_else(poisoned)
                .clone()
                .unwrap_or_else(|| PathBuf::from("/lambo/lambo")))
        }

        fn sleep(&self, duration: Duration) {
            if let Some(hook) = self.sleep_hook.lock().unwrap_or_else(poisoned).clone() {
                hook();
            }
            let pause = duration.max(self.sleep_scale);
            std::thread::sleep(pause.min(Duration::from_millis(80)));
        }

        fn list_processes(&self) -> Result<Vec<crate::zombies::RunningProcess>> {
            Ok(Vec::new())
        }
    }

    /// The scripted host as the engine sees it.
    fn as_host(host: &Arc<ScriptedHost>) -> Arc<dyn ServiceHost> {
        Arc::clone(host) as Arc<dyn ServiceHost>
    }

    /// Gives a fixture file a definite timestamp: files written in the same
    /// instant can share one, and the log ordering then depends on the name.
    fn touch(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("failed to open the fixture");
        file.set_modified(when)
            .expect("failed to set the fixture's timestamp");
    }

    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().unwrap_or_else(poisoned).push(line.to_owned());
        });
        (log, lines)
    }

    /// The states a service reported, as the test's callback collected them.
    type States = Arc<Mutex<Vec<(bool, u32)>>>;

    fn states() -> (StateCallback, States) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let callback: StateCallback = Arc::new(move |running: bool, pid: u32| {
            sink.lock().unwrap_or_else(poisoned).push((running, pid));
        });
        (callback, seen)
    }

    /// Waits for something a background thread is expected to reach.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    fn log_lines(lines: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        lines.lock().unwrap_or_else(poisoned).clone()
    }

    fn nginx(port: u16) -> ServiceConfig {
        ServiceConfig::new("Nginx", r"C:\lambo\bin\nginx\nginx.exe")
            .args(["-p", r"C:\lambo\bin\nginx"])
            .port(port)
    }

    fn postgres(data_dir: &Path) -> ServiceConfig {
        ServiceConfig::new(POSTGRES, r"C:\lambo\bin\pgsql\bin\postgres.exe")
            .args(["-D", &data_dir.display().to_string()])
    }

    /// Starts a service and waits until the scripted postmaster has been
    /// adopted, i.e. until `pid()` reports the PID from the PID file.
    fn started_postgres(
        host: Arc<ScriptedHost>,
        config: ServiceConfig,
        data_dir: &Path,
        log: LogFn,
    ) -> Arc<Service> {
        let dir = data_dir.to_path_buf();
        host.on_sleep(Arc::new(move || {
            // The file appears the first time the engine waits for it, which is
            // when PostgreSQL would have written it.
            let _ = std::fs::write(dir.join(POSTMASTER_PID_FILE), "5566\n");
        }));

        let service = Service::new(as_host(&host), config, log);
        service.start().expect("the launch is not the slow part");
        assert!(
            eventually(|| service.pid() == Some(5566)),
            "the engine must adopt the postmaster"
        );
        service
    }

    // ------------------------------------------------------------- starting

    #[test]
    fn a_second_start_reports_the_running_pid_without_starting_anything() {
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(4242)));
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);

        service.start().expect("the first start succeeds");
        let error = service.start().expect_err("the second start is refused");

        assert_eq!(error.to_string(), "Nginx already running (pid 4242)");
        assert_eq!(host.started().len(), 1, "nothing is started twice");
        assert!(service.running());
        assert_eq!(log_lines(&lines), vec!["[Nginx] started (pid 4242)"]);
    }

    #[test]
    fn a_taken_port_is_refused_before_anything_is_started() {
        let host = Arc::new(ScriptedHost::with_busy_port(3306));
        let service = Service::quiet(as_host(&host), nginx(3306));

        let error = service.start().expect_err("a busy port is refused");

        assert_eq!(error.to_string(), "port 3306 already in use");
        assert!(host.started().is_empty());
        assert!(!service.running());
    }

    #[test]
    fn port_zero_means_the_service_is_not_checked_against_a_port() {
        let host = Arc::new(ScriptedHost::with_busy_port(80));
        let service = Service::quiet(as_host(&host), ServiceConfig::new("Node", "node.exe"));

        service.start().expect("no port, no check");

        assert_eq!(host.started().len(), 1);
    }

    #[test]
    fn a_started_service_logs_its_pid_and_reports_its_state() {
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(4242)));
        let (log, lines) = recorder();
        let (callback, seen) = states();
        let service = Service::new(as_host(&host), nginx(8080), log);
        service.set_state_callback(callback);

        service.start().expect("nginx starts");

        assert_eq!(service.pid(), Some(4242));
        assert_eq!(log_lines(&lines), vec!["[Nginx] started (pid 4242)"]);
        assert_eq!(*seen.lock().unwrap_or_else(poisoned), vec![(true, 4242)]);
    }

    #[test]
    fn the_services_own_output_reaches_the_log_with_its_name_in_front() {
        let host = Arc::new(ScriptedHost::default().replying(
            Reply::alive(4242).speaking("one\ntwo\r\n\n", "listening on 127.0.0.1:80\n"),
        ));
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);

        service.start().expect("nginx starts");

        assert!(eventually(|| log_lines(&lines).len() >= 4));
        let lines = log_lines(&lines);
        assert!(lines.contains(&"[Nginx] one".to_owned()), "{lines:?}");
        // A carriage return is not part of the line, and an empty line is not a
        // line at all.
        assert!(lines.contains(&"[Nginx] two".to_owned()), "{lines:?}");
        assert!(lines.contains(&"[Nginx] listening on 127.0.0.1:80".to_owned()));
        assert!(!lines.iter().any(|line| line == "[Nginx] "), "{lines:?}");
    }

    #[test]
    fn an_exit_is_reported_and_the_service_is_no_longer_running() {
        let gate = Arc::new(Gate::default());
        let host =
            Arc::new(ScriptedHost::default().replying(Reply::alive(4242).held(Arc::clone(&gate))));
        let (log, lines) = recorder();
        let (callback, seen) = states();
        let service = Service::new(as_host(&host), nginx(8080), log);
        service.set_state_callback(callback);

        service.start().expect("nginx starts");
        assert!(service.running());
        assert_eq!(
            log_lines(&lines),
            vec!["[Nginx] started (pid 4242)".to_owned()],
            "nothing but the start while it is still going"
        );

        gate.release();
        assert!(eventually(|| !service.running()));

        assert_eq!(service.pid(), None);
        assert!(log_lines(&lines).contains(&"[Nginx] exited cleanly".to_owned()));
        assert_eq!(
            *seen.lock().unwrap_or_else(poisoned),
            vec![(true, 4242), (false, 0)]
        );
    }

    #[test]
    fn a_failed_exit_is_reported_with_its_reason() {
        let host = Arc::new(ScriptedHost::default().replying(
            Reply::alive(4242).failing_with(WaitOutcome::Failed("exit status 3".to_owned())),
        ));
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);

        service.start().expect("nginx starts");
        assert!(eventually(|| log_lines(&lines).len() >= 2));

        assert!(
            log_lines(&lines).contains(&"[Nginx] exited: exit status 3".to_owned()),
            "{:?}",
            log_lines(&lines)
        );
        assert!(!service.running());
    }

    #[test]
    fn a_start_the_platform_refuses_is_reported_and_leaves_the_service_idle() {
        let host = Arc::new(
            ScriptedHost::default().replying(Reply::Refuses("could not start nginx".to_owned())),
        );
        let service = Service::quiet(as_host(&host), nginx(8080));

        let error = service.start().expect_err("the refusal is reported");

        assert_eq!(error.to_string(), "could not start nginx");
        assert!(!service.running());
        assert_eq!(service.pid(), None);
    }

    // ------------------------------------------------------------- stopping

    #[test]
    fn stop_without_a_running_process_does_nothing() {
        let host = Arc::new(ScriptedHost::default());
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);

        service.stop().expect("stopping nothing is not an error");

        assert!(host.killed().is_empty());
        assert!(log_lines(&lines).is_empty());
    }

    #[test]
    fn stop_terminates_the_process_tree_and_says_so() {
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(4242)));
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);
        service.start().expect("nginx starts");

        service.stop().expect("nginx stops");

        assert_eq!(host.killed(), vec![4242]);
        assert!(host.killed_one().is_empty(), "the tree kill was enough");
        assert!(log_lines(&lines).contains(&"[Nginx] stopping (pid 4242)...".to_owned()));
    }

    #[test]
    fn a_tree_kill_that_fails_falls_back_to_the_single_process() {
        let host = Arc::new(
            ScriptedHost::default()
                .replying(Reply::alive(4242))
                .refusing_tree_kill(),
        );
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), nginx(8080), log);
        service.start().expect("nginx starts");

        service.stop().expect("the fallback succeeds");

        assert!(host.killed().is_empty());
        assert_eq!(host.killed_one(), vec![4242]);
        assert!(log_lines(&lines).contains(&"[Nginx] stopping (pid 4242)...".to_owned()));
    }

    #[test]
    fn a_stop_that_fails_both_ways_names_both_failures() {
        let host = Arc::new(
            ScriptedHost::default()
                .replying(Reply::alive(4242))
                .refusing_every_kill(),
        );
        let service = Service::quiet(as_host(&host), nginx(8080));
        service.start().expect("nginx starts");

        let error = service.stop().expect_err("both failures are reported");

        assert_eq!(
            error.to_string(),
            "kill Nginx: taskkill=taskkill: access denied, \
             proc.Kill=proc.Kill: access denied"
        );
    }

    // ------------------------------------------------------------ postgres

    #[test]
    fn postgres_needs_a_data_directory_in_its_arguments() {
        let host = Arc::new(ScriptedHost::default());
        let service = Service::quiet(as_host(&host), ServiceConfig::new(POSTGRES, "postgres.exe"));

        let error = service
            .start()
            .expect_err("there is nowhere to put a cluster");

        assert_eq!(
            error.to_string(),
            "PostgreSQL data directory (-D) not found in arguments"
        );
        assert!(host.started().is_empty());
        assert!(host.handled_pids().is_empty());
    }

    #[test]
    fn postgres_is_launched_through_runas_with_the_hide_run_wrapper() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)));
        let (log, _lines) = recorder();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);

        service.start().expect("the launch is set up");

        assert!(eventually(|| !host.started().is_empty()));
        let started = host.started();
        let (program, args, piped) = &started[0];
        assert_eq!(program, "runas");
        assert_eq!(
            args,
            &vec![
                "/trustlevel:0x20000".to_owned(),
                format!(
                    "\"/lambo/lambo\" {HIDE_RUN_FLAG} \"{}\" -D \"{}\"",
                    r"C:\lambo\bin\pgsql\bin\postgres.exe",
                    temp.path().display()
                ),
            ]
        );
        assert!(!piped, "the intermediary's output is nobody's business");
    }

    #[test]
    fn a_placeholder_holds_the_service_until_the_postmaster_pid_is_known() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)).slow());
        let (log, _lines) = recorder();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);

        service.start().expect("the launch is set up");

        // The placeholder points at this process, which is what the original's
        // `os.FindProcess(os.Getpid())` did: the handle exists so the service
        // reads as running and so the wait loop ends when the interface does.
        assert!(eventually(|| service.pid() == Some(std::process::id())));
        assert_eq!(
            host.handled_pids().first().copied(),
            Some(std::process::id())
        );
    }

    #[test]
    fn postgres_removes_a_stale_pid_file_before_starting() {
        let temp = TempDir::new();
        let pid_file = temp.path().join(POSTMASTER_PID_FILE);
        std::fs::write(&pid_file, "999\n").expect("failed to write the fixture");
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)));
        let service = Service::quiet(as_host(&host), postgres(temp.path()));

        service.start().expect("the launch is set up");

        assert!(
            !pid_file.exists(),
            "a PID file from a previous run must not be mistaken for this one"
        );
    }

    #[test]
    fn postgres_adopts_the_postmasters_pid_and_reports_it() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)));
        let (log, lines) = recorder();
        let (callback, seen) = states();
        let dir = temp.path().to_path_buf();
        host.on_sleep(Arc::new(move || {
            let _ = std::fs::write(dir.join(POSTMASTER_PID_FILE), "5566\n");
        }));

        // The placeholder holds the service until the postmaster's own PID is
        // written, and then the engine points at that instead.
        let service = Service::new(as_host(&host), postgres(temp.path()), log);
        service.set_state_callback(callback);
        service.start().expect("the launch is set up");

        assert!(eventually(|| service.pid() == Some(5566)));
        assert!(eventually(|| seen
            .lock()
            .unwrap_or_else(poisoned)
            .contains(&(true, 5566))));
        assert!(
            log_lines(&lines)
                .iter()
                .any(|line| line == "[PostgreSQL] started (pid 5566)"),
            "{:?}",
            log_lines(&lines)
        );
        assert!(log_lines(&lines).iter().any(
            |line| line == "[PostgreSQL] launching postgres.exe directly in the background..."
        ));
        assert_eq!(
            host.handled_pids().first().copied(),
            Some(std::process::id())
        );
    }

    #[test]
    fn postgres_without_a_self_exe_runs_the_server_directly() {
        let temp = TempDir::new();
        let host = Arc::new(
            ScriptedHost::default()
                .replying(Reply::alive(0))
                .without_self_exe(),
        );
        let service = Service::quiet(as_host(&host), postgres(temp.path()));

        service.start().expect("the launch is set up");

        assert!(eventually(|| !host.started().is_empty()));
        let started = host.started();
        assert_eq!(started[0].1[0], "/trustlevel:0x20000");
        assert_eq!(
            started[0].1[1],
            format!(
                "\"{}\" -D \"{}\"",
                r"C:\lambo\bin\pgsql\bin\postgres.exe",
                temp.path().display()
            ),
            "without a path to itself, the original ran the server directly"
        );
    }

    #[test]
    fn a_runas_that_cannot_start_is_logged_and_leaves_the_service_idle() {
        let temp = TempDir::new();
        let host = Arc::new(
            ScriptedHost::default().replying(Reply::Refuses("could not run `runas`".to_owned())),
        );
        let (log, lines) = recorder();
        let (callback, seen) = states();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);
        service.set_state_callback(callback);

        service.start().expect("the attempt is made");

        assert!(eventually(|| !log_lines(&lines).is_empty()));
        assert_eq!(
            log_lines(&lines)[0],
            "[PostgreSQL] launching postgres.exe directly in the background..."
        );
        assert!(
            log_lines(&lines)
                .iter()
                .any(|line| line.starts_with("[PostgreSQL] failed to spawn runas: ")),
            "{:?}",
            log_lines(&lines)
        );
        assert!(eventually(|| !service.running()));
        assert_eq!(
            *seen.lock().unwrap_or_else(poisoned),
            vec![(false, 0)],
            "a start that never happened reports itself as not running"
        );
    }

    #[test]
    fn a_pid_file_that_never_appears_is_reported() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)));
        let (log, lines) = recorder();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);

        service.start().expect("the attempt is made");

        assert!(eventually(|| log_lines(&lines).iter().any(
            |line| line.starts_with("[PostgreSQL] failed to read postmaster PID: ")
        )));
        let failure = log_lines(&lines)
            .into_iter()
            .find(|line| line.starts_with("[PostgreSQL] failed to read postmaster PID: "))
            .expect("the failure is logged");
        assert!(failure.contains("postmaster.pid"), "{failure}");
        assert!(!service.running());
    }

    #[test]
    fn a_start_that_no_longer_holds_the_service_does_not_adopt_the_postmaster() {
        let temp = TempDir::new();
        // Slow waits give the test a comfortable window between the `runas`
        // launch and the PID file being read.
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)).slow());
        let dir = temp.path().to_path_buf();
        host.on_sleep(Arc::new(move || {
            let _ = std::fs::write(dir.join(POSTMASTER_PID_FILE), "5566\n");
        }));

        let (log, lines) = recorder();
        let (callback, seen) = states();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);
        service.set_state_callback(callback);

        service.start().expect("the launch is set up");
        assert!(eventually(|| host.started().len() == 1), "the runas launch");

        // A second start replaces the record while the first launch is still
        // polling, which is what the original's own `s.cmd = placeholderCmd`
        // does; the first launch must then leave the postmaster alone.
        let replacement: Arc<dyn HostedProcess> = ScriptedProcess::new(1, WaitOutcome::Clean);
        service.hold(replacement, None);

        std::thread::sleep(Duration::from_millis(50));

        assert!(
            !log_lines(&lines)
                .iter()
                .any(|line| line.contains("started (pid 5566)")),
            "{:?}",
            log_lines(&lines)
        );
        assert!(
            seen.lock().unwrap_or_else(poisoned).is_empty(),
            "a launch that lost the service reports nothing"
        );
        assert_eq!(service.pid(), Some(1), "the newer start is what is held");
    }

    #[test]
    fn postgres_stopped_while_it_is_still_starting_kills_what_it_holds() {
        let temp = TempDir::new();
        // There is no server for `pg_ctl` to stop yet, which is how the original
        // reaches the plain kill in this window.
        let host = Arc::new(
            ScriptedHost::default()
                .replying(Reply::alive(0))
                .capturing(Capture::Run(CapturedRun {
                    success: false,
                    code: Some(3),
                    combined: "pg_ctl: no server running\n".to_owned(),
                    ..CapturedRun::default()
                }))
                .slow(),
        );
        let dir = temp.path().to_path_buf();
        host.on_sleep(Arc::new(move || {
            let _ = std::fs::write(dir.join(POSTMASTER_PID_FILE), "5566\n");
        }));
        let (log, _) = recorder();
        let service = Service::new(as_host(&host), postgres(temp.path()), log);

        service.start().expect("the launch is set up");
        assert!(eventually(|| !host.started().is_empty()));

        // Until the postmaster's PID is known, the service holds the
        // placeholder, which the original pointed at its own process: stopping
        // in that window stops that and nothing else. `Stop` does not clear the
        // record either; the wait does, and this launch is left without one.
        service.stop().expect("the service is stopped");
        assert_eq!(host.killed(), vec![std::process::id()]);
        assert_eq!(
            service.pid(),
            Some(std::process::id()),
            "the placeholder is still held: `Stop` does not clear the record"
        );
    }

    #[test]
    fn postgres_stop_asks_pg_ctl_first_and_does_not_kill_anything() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)));
        let (log, lines) = recorder();
        let service = started_postgres(Arc::clone(&host), postgres(temp.path()), temp.path(), log);

        service.stop().expect("pg_ctl stops it");

        assert_eq!(host.killed(), Vec::<u32>::new());
        assert!(
            log_lines(&lines)
                .contains(&"[PostgreSQL] stopping (pid 5566) via pg_ctl...".to_owned())
        );
        let captured = host.capture_specs();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].program.file_name(),
            Some(std::ffi::OsStr::new(PG_CTL_PROGRAM)),
            "the scripted server is spelled as a Windows path, which is one              component on this platform; the sibling rule is asserted in \
             `pg_ctl_sits_next_to_the_server`"
        );
        assert_eq!(
            captured[0].args,
            vec![
                "stop".to_owned(),
                "-D".to_owned(),
                temp.path().display().to_string(),
                "-m".to_owned(),
                "fast".to_owned(),
            ]
        );
    }

    #[test]
    fn postgres_stop_falls_through_to_the_tree_kill_when_pg_ctl_fails() {
        let temp = TempDir::new();
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(0)).capturing(
            Capture::Run(CapturedRun {
                success: false,
                code: Some(3),
                combined: "pg_ctl: no server running\n".to_owned(),
                ..CapturedRun::default()
            }),
        ));
        let (log, lines) = recorder();
        let service = started_postgres(Arc::clone(&host), postgres(temp.path()), temp.path(), log);

        service.stop().expect("the fallback stops it");

        assert_eq!(host.killed(), vec![5566]);
        let lines = log_lines(&lines);
        assert!(
            lines.contains(&format!(
                "[PostgreSQL] pg_ctl stop failed: exit status 3 (output: {:?})",
                "pg_ctl: no server running\n"
            )),
            "{lines:?}"
        );
        assert!(lines.contains(&"[PostgreSQL] stopping (pid 5566)...".to_owned()));
    }

    #[test]
    fn postgres_stop_reports_a_pg_ctl_that_could_not_be_run() {
        let temp = TempDir::new();
        let host = Arc::new(
            ScriptedHost::default()
                .replying(Reply::alive(0))
                .capturing(Capture::Refuses("could not run `pg_ctl.exe`".to_owned())),
        );
        let (log, lines) = recorder();
        let service = started_postgres(Arc::clone(&host), postgres(temp.path()), temp.path(), log);

        service.stop().expect("the fallback stops it");

        assert_eq!(host.killed(), vec![5566]);
        let refusal = r#"[PostgreSQL] pg_ctl stop failed: could not run `pg_ctl.exe` (output: "")"#;
        assert!(
            log_lines(&lines).iter().any(|line| line == refusal),
            "{:?}",
            log_lines(&lines)
        );
    }

    #[test]
    fn pg_ctl_sits_next_to_the_server() {
        // A real path with this platform's separators: the scripted server in
        // the other tests is a Windows spelling, which is one component here.
        let server = std::env::temp_dir()
            .join("lambo")
            .join("pgsql")
            .join("bin")
            .join("postgres.exe");

        let config = ServiceConfig::new(POSTGRES, &server);

        assert_eq!(config.pg_ctl_path(), server.with_file_name(PG_CTL_PROGRAM));
    }

    #[test]
    fn the_data_directory_is_the_one_after_a_dash_d() {
        assert_eq!(
            postgres_data_dir(&["-D".to_owned(), "C:/data".to_owned()]),
            Some(PathBuf::from("C:/data"))
        );
        assert_eq!(
            postgres_data_dir(&[
                "-p".to_owned(),
                "5432".to_owned(),
                "-D".to_owned(),
                "d".to_owned()
            ]),
            Some(PathBuf::from("d"))
        );
        assert_eq!(
            postgres_data_dir(&["-D".to_owned()]),
            None,
            "the flag has no value"
        );
        assert_eq!(postgres_data_dir(&[]), None);
        assert_eq!(
            postgres_data_dir(&["-Data".to_owned()]),
            None,
            "only the flag itself counts, not a word that starts like it"
        );
    }

    // ------------------------------------------------------ postmaster.pid

    #[test]
    fn the_pid_is_the_first_line_of_the_pid_file() {
        assert_eq!(parse_postmaster_pid("5566\n"), Ok(5566));
        assert_eq!(parse_postmaster_pid("5566"), Ok(5566));
        assert_eq!(parse_postmaster_pid("  5566  \nC:/data\n"), Ok(5566));
    }

    #[test]
    fn a_pid_file_without_a_number_says_so() {
        let error = parse_postmaster_pid("not a pid\n").expect_err("a number is required");
        assert!(
            error.starts_with("invalid PID in postmaster.pid: "),
            "{error}"
        );
    }

    #[test]
    fn an_empty_pid_file_is_reported_as_empty_once_polling_gives_up() {
        let temp = TempDir::new();
        std::fs::write(temp.path().join(POSTMASTER_PID_FILE), "")
            .expect("failed to write the fixture");
        let host = ScriptedHost::default();

        let error = read_postmaster_pid(&host, temp.path()).expect_err("empty is not a PID");
        assert_eq!(error, "empty postmaster.pid");
    }

    #[test]
    fn the_pid_file_is_polled_until_postgres_writes_it() {
        let temp = TempDir::new();
        let dir = temp.path().to_path_buf();
        let host = ScriptedHost::default();
        // Written on the third wait, as PostgreSQL would.
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        host.on_sleep(Arc::new(move || {
            if counter.fetch_add(1, Ordering::SeqCst) >= 2 {
                let _ = std::fs::write(dir.join(POSTMASTER_PID_FILE), "5566\n");
            }
        }));

        assert_eq!(read_postmaster_pid(&host, temp.path()), Ok(5566));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    // -------------------------------------------------------------- the log

    #[test]
    fn a_postgres_log_line_keeps_only_its_level_and_message() {
        assert_eq!(
            postgres_log_line("2026-01-01 10:00:00.123 GMT [1234] LOG:  database system is ready"),
            "LOG: database system is ready"
        );
        assert_eq!(
            postgres_log_line("2026-01-01 10:00:00 GMT [1234] ERROR:  relation does not exist"),
            "ERROR: relation does not exist"
        );
    }

    #[test]
    fn a_line_without_a_level_is_passed_through() {
        for line in [
            "no level here",
            "2026-01-01 10:00:00 GMT [1234] LOG: single space after the colon",
            "LOG: ",
        ] {
            assert_eq!(postgres_log_line(line), line);
        }
    }

    #[test]
    fn the_log_file_is_the_one_current_logfiles_names() {
        let temp = TempDir::new();
        std::fs::write(
            temp.path().join(CURRENT_LOGFILES),
            "stderr log/postgresql-2026-01-01_000000.log\ncsvlog log/x.csv\n",
        )
        .expect("failed to write the fixture");

        assert_eq!(
            log_file_from_current_logfiles(temp.path()),
            Some(temp.path().join("log/postgresql-2026-01-01_000000.log"))
        );
    }

    #[test]
    fn current_logfiles_without_a_stderr_line_names_nothing() {
        let temp = TempDir::new();
        std::fs::write(temp.path().join(CURRENT_LOGFILES), "csvlog log/x.csv\n")
            .expect("failed to write the fixture");
        assert_eq!(log_file_from_current_logfiles(temp.path()), None);
        assert_eq!(
            log_file_from_current_logfiles(&temp.path().join("nowhere")),
            None
        );
    }

    #[test]
    fn the_newest_log_is_the_fallback() {
        let temp = TempDir::new();
        let log_dir = temp.path().join(LOG_SUBDIR);
        std::fs::create_dir_all(&log_dir).expect("failed to create the fixture");
        let old = log_dir.join("old.log");
        let newest = log_dir.join("new.log");
        std::fs::write(&old, "old").expect("failed to write the fixture");
        std::fs::write(log_dir.join("ignored.txt"), "not a log").expect("failed to write it");
        std::fs::create_dir_all(log_dir.join("directory.log")).expect("failed to create it");
        std::fs::write(&newest, "new").expect("failed to write the fixture");
        let now = std::time::SystemTime::now();
        touch(&newest, now);
        touch(&old, now - Duration::from_secs(60));

        assert_eq!(newest_log_file(temp.path()), Some(newest));
        assert_eq!(newest_log_file(&temp.path().join("nowhere")), None);
    }

    #[test]
    fn logs_written_in_the_same_instant_are_taken_in_name_order() {
        let temp = TempDir::new();
        let log_dir = temp.path().join(LOG_SUBDIR);
        std::fs::create_dir_all(&log_dir).expect("failed to create the fixture");
        let first = log_dir.join("aaa.log");
        let second = log_dir.join("zzz.log");
        std::fs::write(&first, "first").expect("failed to write the fixture");
        std::fs::write(&second, "second").expect("failed to write the fixture");
        let same = std::time::SystemTime::now();
        touch(&first, same);
        touch(&second, same);

        // The original walked `os.ReadDir`, which sorts by name, and kept the
        // first file that was not older than the best so far.
        assert_eq!(newest_log_file(temp.path()), Some(first));
    }

    #[test]
    fn the_log_is_looked_for_until_it_appears() {
        let temp = TempDir::new();
        let dir = temp.path().to_path_buf();
        let host = ScriptedHost::default();
        let stop = AtomicBool::new(false);
        let made = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&made);
        host.on_sleep(Arc::new(move || {
            if !flag.swap(true, Ordering::SeqCst) {
                let log_dir = dir.join(LOG_SUBDIR);
                let _ = std::fs::create_dir_all(&log_dir);
                let _ = std::fs::write(log_dir.join("postgresql.log"), "found\n");
            }
        }));

        let found = find_postgres_log(&host, &stop, temp.path());
        assert_eq!(
            found,
            Some(temp.path().join(LOG_SUBDIR).join("postgresql.log"))
        );
    }

    #[test]
    fn a_stopped_search_for_the_log_gives_up_quietly() {
        let temp = TempDir::new();
        let host = ScriptedHost::default();
        let stop = AtomicBool::new(true);
        assert_eq!(find_postgres_log(&host, &stop, temp.path()), None);
    }

    #[test]
    fn the_follower_reports_complete_lines_and_holds_a_partial_one_back() {
        let temp = TempDir::new();
        let path = temp.path().join("postgresql.log");
        std::fs::write(&path, "first\nsecond\r\nthird without a newline")
            .expect("failed to write the fixture");
        let stop = Arc::new(AtomicBool::new(false));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let for_sink = Arc::clone(&stop);

        let result = follow_log(
            &path,
            &stop,
            &move |line: &str| {
                let mut collected = sink_lines.lock().unwrap_or_else(poisoned);
                collected.push(line.to_owned());
                // Both complete lines are in and the third has no ending yet:
                // the follower is stopped, and that third line must never be
                // reported.
                if collected.len() == 2 {
                    drop(collected);
                    for_sink.store(true, Ordering::SeqCst);
                }
            },
            &|_| std::thread::yield_now(),
        );

        assert!(result.is_ok());
        assert_eq!(
            *lines.lock().unwrap_or_else(poisoned),
            vec!["first".to_owned(), "second".to_owned()],
            "an unfinished line is not reported"
        );
    }

    #[test]
    fn the_follower_stops_when_it_is_asked_to() {
        let temp = TempDir::new();
        let path = temp.path().join("postgresql.log");
        std::fs::write(&path, "").expect("failed to write the fixture");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);

        let result = follow_log(
            &path,
            &stop,
            &move |_line: &str| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
            &move |_| {
                // The log is closed while the follower is waiting for it.
                flag.store(true, Ordering::SeqCst);
            },
        );

        assert!(result.is_ok());
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_missing_log_file_is_an_error_the_caller_reports() {
        let temp = TempDir::new();
        let stop = AtomicBool::new(false);
        assert!(
            follow_log(&temp.path().join("gone.log"), &stop, &|_| {}, &|_| {}).is_err(),
            "the caller says `warning: failed to open PostgreSQL log file`"
        );
    }

    #[test]
    fn a_stream_reports_a_last_line_that_has_no_newline() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        stream_to_log(
            Cursor::new(b"one\r\ntwo\nthree".to_vec()),
            &move |line: &str| {
                sink_lines
                    .lock()
                    .unwrap_or_else(poisoned)
                    .push(line.to_owned());
            },
        );
        assert_eq!(
            *lines.lock().unwrap_or_else(poisoned),
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]
        );
    }

    #[test]
    fn a_stream_that_ends_mid_line_still_reports_the_line_it_has() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        stream_to_log(
            Cursor::new(b"kept\nkept too".to_vec()),
            &move |line: &str| {
                sink_lines
                    .lock()
                    .unwrap_or_else(poisoned)
                    .push(line.to_owned());
            },
        );
        assert_eq!(log_lines(&lines).len(), 2);
    }

    // ------------------------------------------------------------ hide-run

    #[test]
    fn hide_run_runs_the_program_with_the_rest_of_the_arguments() {
        let host = Arc::new(ScriptedHost::default().replying(Reply::alive(77)));
        let argv = vec![
            HIDE_RUN_FLAG.to_owned(),
            r"C:\lambo\bin\pgsql\bin\postgres.exe".to_owned(),
            "-D".to_owned(),
            r"C:\data".to_owned(),
        ];

        let outcome = hide_run(&argv, host.as_ref()).expect("the flag is there");

        outcome.expect("the program starts");
        let started = host.started_specs();
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].program,
            PathBuf::from(r"C:\lambo\bin\pgsql\bin\postgres.exe")
        );
        assert_eq!(
            started[0].args,
            vec!["-D".to_owned(), r"C:\data".to_owned()]
        );
        assert!(started[0].detached, "nothing waits for it");
        assert_eq!(started[0].stdout, crate::process::Output::Null);
    }

    #[test]
    fn an_ordinary_command_line_is_not_a_hide_run() {
        assert!(hide_run_from_argv(&["up".to_owned()]).is_none());
        assert!(hide_run_from_argv(&[]).is_none());
        // The flag needs a program to run: the original asked for at least one
        // argument after it, i.e. `len(os.Args) >= 3`.
        assert!(hide_run_from_argv(&[HIDE_RUN_FLAG.to_owned()]).is_none());
    }

    #[test]
    fn hide_run_says_nothing_for_anything_else() {
        let host = Arc::new(ScriptedHost::default());
        for argv in [
            Vec::new(),
            vec![HIDE_RUN_FLAG.to_owned()],
            vec!["--tray".to_owned(), "lambo".to_owned()],
            vec!["lambo".to_owned(), HIDE_RUN_FLAG.to_owned()],
        ] {
            assert!(
                hide_run(&argv, host.as_ref()).is_none(),
                "{argv:?} is not the flag"
            );
        }
        assert!(host.started().is_empty());
    }

    // ----------------------------------------------------------- runas text

    #[test]
    fn the_runas_command_line_quotes_every_path() {
        assert_eq!(
            runas_command_line(
                Some(Path::new(r"C:\Program Files\Lambo\lambo.exe")),
                Path::new(r"C:\Program Files\Lambo\bin\pgsql\bin\postgres.exe"),
                Path::new(r"C:\Program Files\data")
            ),
            format!(
                "\"C:\\Program Files\\Lambo\\lambo.exe\" {HIDE_RUN_FLAG} \
                 \"C:\\Program Files\\Lambo\\bin\\pgsql\\bin\\postgres.exe\" \
                 -D \"C:\\Program Files\\data\""
            )
        );
    }

    #[test]
    fn the_runas_invocation_asks_for_a_restricted_token() {
        let spec = runas_spec("cmd", Some(Path::new(r"C:\data")));
        assert_eq!(spec.program, PathBuf::from("runas"));
        assert_eq!(spec.args, vec!["/trustlevel:0x20000", "cmd"]);
        assert_eq!(spec.cwd, Some(PathBuf::from(r"C:\data")));
        assert_eq!(runas_spec("cmd", None).cwd, None);
    }
}
