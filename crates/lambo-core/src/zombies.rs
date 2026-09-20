//! The startup sweep: children a previous run left behind.
//!
//! Ported from the original implementation's `zombies.go`. A stack that was not
//! shut down properly -
//! the interface was killed, the machine lost power, a server crashed - leaves
//! `httpd.exe`, `mysqld.exe` and friends running, holding the ports the next run
//! needs. The original asked Windows for every process with a path, kept the
//! ones living under `<base>\bin\` (which is where Lambo puts all of its own
//! programs, and nowhere else), and terminated each one with its tree.
//!
//! # What is deliberately narrowed
//!
//! The original could not tell "a child of a crashed run" from "a server the
//! user started five seconds ago", and this engine can: the services it runs are
//! held by the stack's engines, which know their process. [`sweep`] therefore
//! takes the set of PIDs to leave alone, and the caller passes the ones its
//! engines are running - a second interface, or a `lambo up` next to a running
//! GUI, no longer kills a healthy stack. Everything else is as it was, including what the sweep does *not*
//! know: a program under `<base>\bin\` that Lambo did not start is still
//! terminated, because that is what the original did and because a stale server
//! is the failure this exists to prevent.
//!
//! # Why PowerShell, and why it is not a shell string
//!
//! The original ran a `Get-Process` pipeline through `powershell.exe`. There is
//! no cheaper way to enumerate processes on Windows without linking the
//! `Win32_System_Diagnostics_ToolHelp` API, which is what that would cost: a
//! second FFI surface for a call that runs once per launch. The script is passed
//! as one argument in an argument vector - never concatenated into a command
//! line, and never built from anything a user typed - so no quoting rule is
//! involved.
//!
//! On Unix the same enumeration reads `/proc`, which is an extension rather than
//! a port: the original had no Unix build. The filtering, the killing and the
//! logging are shared.

use std::path::{Path, PathBuf};

use crate::process::ProcessSpec;
use crate::service::ServiceHost;

/// The program the sweep enumerates processes with, on Windows.
pub const POWERSHELL: &str = "powershell.exe";

/// The `Get-Process` pipeline the original ran.
///
/// Every process that has a path (a `System` process has none), as
/// `<pid>|<path>` lines.
pub const PROCESS_LIST_SCRIPT: &str = "Get-Process | Where-Object { $_.Path } | \
     ForEach-Object { \"$($_.Id)|$($_.Path)\" }";

/// One process the enumeration reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningProcess {
    /// Its process id.
    pub pid: u32,
    /// The executable it is running.
    pub path: PathBuf,
}

/// The invocation that lists processes.
///
/// `-NoProfile -NonInteractive` keeps a user's PowerShell profile from changing
/// the answer or hanging the launch, exactly as the original had it.
pub fn process_list_spec() -> ProcessSpec {
    ProcessSpec::new(POWERSHELL, "process sweep")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(PROCESS_LIST_SCRIPT)
}

/// Parses the `<pid>|<path>` lines the enumeration produces.
///
/// A line without a separator, without a PID, or without a path is skipped:
/// the enumeration's shape is the only thing this can rely on, and a guess
/// would be a process killed on a misreading.
pub fn parse_process_table(text: &str) -> Vec<RunningProcess> {
    let mut processes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((pid, path)) = line.split_once('|') else {
            continue;
        };
        let (pid, path) = (pid.trim(), path.trim());
        if pid.is_empty() || path.is_empty() {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        processes.push(RunningProcess {
            pid,
            path: PathBuf::from(path),
        });
    }
    processes
}

/// `path` in the form the sweep compares: lower case, forward slashes.
///
/// Windows paths are case-insensitive and accept either separator, so the
/// original normalised both sides before comparing prefixes.
pub fn normalised(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// The processes living under `<base_dir>/bin/`.
///
/// That directory is where every program Lambo installs goes, and the prefix is
/// the original's: a process running an executable from there is one of Lambo's
/// own, whether it belongs to a crashed run or to one that is still going (see
/// [`sweep`] for how a running stack is protected).
pub fn stale_processes(base_dir: &Path, processes: &[RunningProcess]) -> Vec<RunningProcess> {
    let needle = format!("{}/bin/", normalised(base_dir));
    processes
        .iter()
        .filter(|process| normalised(&process.path).starts_with(&needle))
        .cloned()
        .collect()
}

/// Terminates the stale processes and describes what it did.
///
/// `keep` holds the PIDs that are known to be running *now* - the ones the
/// state file says this installation owns - and they are left alone.
///
/// Returns the lines the interface logs, in the original's words:
/// `startup sweep: killed stale <path> (pid <pid>)`. A process that refused to
/// die is not reported, which is what the original did with `taskkill`'s exit
/// status: it could fail because the process had already gone.
pub fn sweep(host: &dyn ServiceHost, base_dir: &Path, keep: &[u32]) -> Vec<String> {
    // The host reports a failed enumeration as an empty table; this is the
    // second belt, for an implementation that reports it as an error.
    let Ok(processes) = host.list_processes() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for process in stale_processes(base_dir, &processes) {
        if keep.contains(&process.pid) {
            continue;
        }
        if host.kill_tree(process.pid).is_ok() {
            lines.push(format!(
                "startup sweep: killed stale {} (pid {})",
                process.path.display(),
                process.pid
            ));
        }
    }
    lines
}

/// Reads the process table on a Unix host, from `/proc`.
///
/// Every process whose executable this user may inspect. A process is
/// identified by the target of its `/proc/<pid>/exe` link, which is the
/// equivalent of the `Path` the Windows enumeration reports.
/// The same table on a Windows host, where there is no `/proc` to read.
///
/// The only caller reaches here when it was *told* the platform is not Windows
/// while running on Windows - a combination only a test can produce, since every
/// other caller passes `Os::host()`. An empty table is what the Unix reader
/// answers with when `/proc` is missing, and what the caller's own failure path
/// already produces, so nothing behaves differently.
#[cfg(windows)]
pub fn proc_processes() -> Vec<RunningProcess> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn proc_processes() -> Vec<RunningProcess> {
    let mut processes = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return processes;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(path) = std::fs::read_link(entry.path().join("exe")) else {
            // Another user's process, or one that has already exited.
            continue;
        };
        processes.push(RunningProcess {
            pid,
            path: strip_deleted_suffix(path),
        });
    }
    processes
}

/// Removes the ` (deleted)` marker the kernel appends to a replaced executable.
#[cfg(not(windows))]
fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    match path.to_string_lossy().strip_suffix(" (deleted)") {
        Some(trimmed) => PathBuf::from(trimmed),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use std::io::Read;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::error::Result;
    use crate::service::{HostedProcess, ServiceHost, WaitOutcome};

    /// A host whose process table and kills are scripted.
    #[derive(Default)]
    struct ScriptedHost {
        processes: Vec<RunningProcess>,
        killed: Mutex<Vec<u32>>,
        refuses: bool,
    }

    impl ServiceHost for ScriptedHost {
        fn port_busy(&self, _port: u16) -> bool {
            false
        }

        fn start(&self, _spec: &ProcessSpec, _piped: bool) -> Result<Arc<dyn HostedProcess>> {
            Err(crate::error::Error::InvalidInput("not used".to_owned()))
        }

        fn process_handle(&self, pid: u32) -> Arc<dyn HostedProcess> {
            Arc::new(NoProcess { pid })
        }

        fn kill_tree(&self, pid: u32) -> Result<()> {
            if self.refuses {
                return Err(crate::error::Error::InvalidInput("no".to_owned()));
            }
            self.killed.lock().expect("kill lock").push(pid);
            Ok(())
        }

        fn kill_process(&self, _pid: u32) -> Result<()> {
            Ok(())
        }

        fn run_captured(&self, _spec: &ProcessSpec) -> Result<crate::service::CapturedRun> {
            Ok(crate::service::CapturedRun::default())
        }

        fn self_exe(&self) -> Result<PathBuf> {
            Ok(PathBuf::from("/lambo/lambo"))
        }

        fn sleep(&self, _duration: Duration) {}

        fn list_processes(&self) -> Result<Vec<RunningProcess>> {
            Ok(self.processes.clone())
        }
    }

    struct NoProcess {
        pid: u32,
    }

    impl HostedProcess for NoProcess {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn wait(&self) -> WaitOutcome {
            WaitOutcome::Clean
        }

        fn take_stdout(&self) -> Option<Box<dyn Read + Send>> {
            None
        }

        fn take_stderr(&self) -> Option<Box<dyn Read + Send>> {
            None
        }
    }

    fn process(pid: u32, path: &str) -> RunningProcess {
        RunningProcess {
            pid,
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn the_enumerations_lines_are_split_into_pid_and_path() {
        let table =
            "4242|C:\\lambo\\bin\\apache\\bin\\httpd.exe\r\n\r\n  77 | D:\\weird path\\a b.exe \n";
        assert_eq!(
            parse_process_table(table),
            vec![
                process(4242, r"C:\lambo\bin\apache\bin\httpd.exe"),
                process(77, r"D:\weird path\a b.exe"),
            ]
        );
    }

    #[test]
    fn a_line_the_enumeration_could_not_have_meant_is_skipped() {
        // No separator, no PID, no path: each of these would otherwise become a
        // process killed on a misreading.
        let table = "no separator here\nnotanumber|C:\\x.exe\n|\n123|\n|C:\\x.exe\n";
        assert!(parse_process_table(table).is_empty());
    }

    #[test]
    fn only_processes_under_the_installations_bin_directory_are_stale() {
        let processes = vec![
            process(1, r"C:\lambo\bin\apache\bin\httpd.exe"),
            process(2, r"C:\lambo\bin\php\php-cgi.exe"),
            process(3, r"C:\lambo\binary\other.exe"),
            process(4, r"C:\other\bin\httpd.exe"),
            process(5, r"C:\lambo\bin"),
        ];

        assert_eq!(
            stale_processes(Path::new(r"C:\lambo"), &processes)
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn the_prefix_comparison_ignores_case_and_separators() {
        let processes = vec![
            process(1, "c:/LAMBO/Bin/Nginx/nginx.exe"),
            process(2, r"C:\Lambo\BIN\php\php-cgi.exe"),
        ];

        assert_eq!(
            stale_processes(Path::new(r"C:\LAMBO"), &processes).len(),
            2,
            "case and separator spelling do not matter on Windows paths"
        );
    }

    #[test]
    fn the_sweep_kills_what_it_finds_and_says_so() {
        let host = ScriptedHost {
            processes: vec![
                process(4242, r"C:\lambo\bin\nginx\nginx.exe"),
                process(7, r"C:\elsewhere\tool.exe"),
            ],
            ..ScriptedHost::default()
        };

        let lines = sweep(&host, Path::new(r"C:\lambo"), &[]);
        assert_eq!(
            *host.killed.lock().expect("kill lock"),
            vec![4242],
            "only the installation's own process is terminated"
        );
        assert_eq!(
            lines,
            vec![r"startup sweep: killed stale C:\lambo\bin\nginx\nginx.exe (pid 4242)".to_owned()]
        );
    }

    #[test]
    fn a_running_stack_is_left_alone() {
        // The state file says these two are alive right now, so the sweep must
        // not terminate them - that is the difference between "a stale child"
        // and "the stack the user is using".
        let host = ScriptedHost {
            processes: vec![
                process(11, r"C:\lambo\bin\apache\bin\httpd.exe"),
                process(12, r"C:\lambo\bin\php\php-cgi.exe"),
                process(13, r"C:\lambo\bin\nginx\nginx.exe"),
            ],
            ..ScriptedHost::default()
        };

        let lines = sweep(&host, Path::new(r"C:\lambo"), &[11, 12]);
        assert_eq!(*host.killed.lock().expect("kill lock"), vec![13]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("(pid 13)"), "{lines:?}");
    }

    #[test]
    fn a_process_that_refuses_to_die_is_not_reported_as_killed() {
        let host = ScriptedHost {
            processes: vec![process(5, r"C:\lambo\bin\redis\redis-server.exe")],
            refuses: true,
            ..ScriptedHost::default()
        };

        assert!(sweep(&host, Path::new(r"C:\lambo"), &[]).is_empty());
        assert!(host.killed.lock().expect("kill lock").is_empty());
    }

    #[test]
    fn an_empty_process_table_kills_nothing() {
        let host = ScriptedHost::default();
        assert!(sweep(&host, Path::new("/lambo"), &[]).is_empty());
    }

    #[test]
    fn the_powershell_invocation_is_the_originals() {
        let spec = process_list_spec();
        assert_eq!(spec.program, PathBuf::from("powershell.exe"));
        assert_eq!(
            spec.args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                PROCESS_LIST_SCRIPT
            ]
        );
        assert!(PROCESS_LIST_SCRIPT.starts_with("Get-Process | Where-Object { $_.Path }"));
    }

    #[test]
    fn the_sweep_sees_its_own_process_table_on_unix() {
        // Not a fixture: on a Unix host this is the real `/proc`, and the test's
        // own process is in it. It proves the enumeration returns something the
        // prefix filter can work with, which is all the platform seam has to do.
        #[cfg(not(windows))]
        {
            let processes = proc_processes();
            assert!(
                processes
                    .iter()
                    .any(|process| process.pid == std::process::id()),
                "the running test process must appear in /proc"
            );
            assert!(
                processes
                    .iter()
                    .all(|process| !process.path.as_os_str().is_empty())
            );

            // And the filter refuses everything under a base directory that
            // cannot be where those executables live.
            assert!(stale_processes(Path::new("/lambo/does/not/exist"), &processes).is_empty());
        }
    }

    #[test]
    fn a_deleted_executable_loses_its_marker() {
        #[cfg(not(windows))]
        {
            assert_eq!(
                strip_deleted_suffix(PathBuf::from("/tmp/tool.exe (deleted)")),
                PathBuf::from("/tmp/tool.exe")
            );
            assert_eq!(
                strip_deleted_suffix(PathBuf::from("/tmp/tool.exe")),
                PathBuf::from("/tmp/tool.exe")
            );
        }
    }

    #[test]
    fn every_stale_process_is_terminated_once_each() {
        let host = ScriptedHost {
            processes: vec![
                process(2, r"C:\lambo\bin\a.exe"),
                process(9, r"C:\lambo\bin\b.exe"),
            ],
            ..ScriptedHost::default()
        };
        let lines = sweep(&host, Path::new(r"C:\lambo"), &[]);
        assert_eq!(*host.killed.lock().expect("kill lock"), vec![2, 9]);
        assert_eq!(lines.len(), 2);
    }
}
