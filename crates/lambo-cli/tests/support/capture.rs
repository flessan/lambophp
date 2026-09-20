//! Pipe capture with separately observable process-exit and pipe-EOF boundaries.
//!
//! Like `Command::output`, success requires BOTH exit and complete output.
//! A timeout is a test failure, not permission to ignore an inherited writer.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

enum Event {
    Bytes(bool, Vec<u8>),
    Eof(bool),
    Exited(ExitStatus),
    Failed(&'static str, io::Error),
}

fn read_pipe(mut pipe: impl Read, stdout: bool, sender: Sender<Event>) {
    let mut buffer = [0; 8192];
    loop {
        let event = match pipe.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(Event::Eof(stdout));
                return;
            }
            Ok(n) => Event::Bytes(stdout, buffer[..n].to_vec()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(Event::Failed(
                    if stdout {
                        "reading stdout"
                    } else {
                        "reading stderr"
                    },
                    error,
                ));
                return;
            }
        };
        if sender.send(event).is_err() {
            return;
        }
    }
}

pub(super) fn output(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawning CLI: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    std::thread::spawn(move || read_pipe(stdout, true, stdout_sender));
    let stderr_sender = sender.clone();
    std::thread::spawn(move || read_pipe(stderr, false, stderr_sender));
    std::thread::spawn(move || {
        let event = match child.wait() {
            Ok(status) => Event::Exited(status),
            Err(error) => Event::Failed("waiting for CLI exit", error),
        };
        let _ = sender.send(event);
    });
    // Do not join a blocked reader on failure: that would hide this deadline.
    // Neither the CLI nor its service is killed to manufacture completion.
    collect(&receiver, deadline)
}

fn collect(receiver: &Receiver<Event>, deadline: Instant) -> Result<Output, String> {
    let mut status = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let failure = loop {
        if let Some(status) = status {
            if stdout_eof && stderr_eof {
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
        }
        if Instant::now() >= deadline {
            break "deadline elapsed capturing CLI output".to_owned();
        }
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Bytes(true, bytes)) => stdout.extend(bytes),
            Ok(Event::Bytes(false, bytes)) => stderr.extend(bytes),
            Ok(Event::Eof(true)) => stdout_eof = true,
            Ok(Event::Eof(false)) => stderr_eof = true,
            Ok(Event::Exited(value)) => status = Some(value),
            Ok(Event::Failed(phase, error)) => break format!("{phase}: {error}"),
            Err(error) => break format!("capturing CLI output: {error}"),
        }
    };
    Err(format!(
        "{failure}\nchild exit: {status:?}\nstdout EOF: {stdout_eof}\nstderr EOF: {stderr_eof}\n\
         --- stdout so far ---\n{}\n--- stderr so far ---\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success() -> ExitStatus {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[test]
    fn child_exit_does_not_substitute_for_pipe_eof() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Event::Bytes(true, b"Ready".to_vec())).unwrap();
        sender.send(Event::Exited(success())).unwrap();
        sender.send(Event::Eof(false)).unwrap();
        // Model an exited CLI with a descendant still holding stdout open.
        // Keep the sender alive, but supply no stdout EOF. No sleeps or child
        // termination are needed to reproduce the capture-state boundary.
        let error = collect(&receiver, Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert!(error.contains("child exit: Some("), "{error}");
        assert!(error.contains("stdout EOF: false"), "{error}");
        assert!(error.contains("stderr EOF: true"), "{error}");
        assert!(error.contains("Ready"), "{error}");
    }

    #[test]
    fn pipe_eof_does_not_substitute_for_child_exit() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Event::Eof(true)).unwrap();
        sender.send(Event::Eof(false)).unwrap();
        let error = collect(&receiver, Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert!(error.contains("child exit: None"), "{error}");
        assert!(error.contains("stdout EOF: true"), "{error}");
        assert!(error.contains("stderr EOF: true"), "{error}");
    }

    #[test]
    fn exit_and_both_eofs_return_captured_bytes() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Event::Exited(success())).unwrap();
        sender
            .send(Event::Bytes(false, b"diagnostic".to_vec()))
            .unwrap();
        sender.send(Event::Eof(false)).unwrap();
        sender.send(Event::Bytes(true, b"Ready".to_vec())).unwrap();
        sender.send(Event::Eof(true)).unwrap();
        let output = collect(&receiver, Instant::now() + Duration::from_millis(50)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"Ready");
        assert_eq!(output.stderr, b"diagnostic");
    }
}
