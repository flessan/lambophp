//! Private Win32 boundary: process spawning, the registry and elevation.
//!
//! [`win32`] holds the calls that are not about processes - the user `PATH` in
//! `HKCU\Environment`, the machine `PATH`, and the UAC prompt. The rest of the
//! crate is the service launcher described below.
//!
//! `STARTF_USESTDHANDLES` selects stdio; it does NOT restrict handle inheritance.
//! Rust's stable Command API at our MSRV cannot supply a handle allowlist. Use
//! STARTUPINFOEX + PROC_THREAD_ATTRIBUTE_HANDLE_LIST, never toggle the parent's
//! existing handle flags, and never fall back to unrestricted inheritance.
//!
//! The rule binds every child that outlives its launcher - a detached service
//! and a piped service alike. Anything either one inherits stays open until the
//! service stops, long after the interface that started it has exited: an
//! unrelated capture pipe handed to a service holds its reader hostage until
//! the service dies. [`spawn`] and [`spawn_piped`] are the only two doors out.
//!
//! All pointer-bearing Win32 calls are confined here. OwnedHandle and the
//! attribute-list guard release resources on both success and error paths.

#![cfg(windows)]

pub mod win32;

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Globalization::CompareStringOrdinal;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::*;

/// An owned service process. Dropping it closes the handle, not the service.
#[derive(Debug)]
pub struct Child {
    handle: OwnedHandle,
    pid: u32,
}

impl From<std::process::Child> for Child {
    fn from(child: std::process::Child) -> Self {
        let pid = child.id();
        Self {
            handle: child.into(),
            pid,
        }
    }
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(0)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_for(INFINITE)?
            .ok_or_else(|| io::Error::other("infinite wait timed out"))
    }

    fn wait_for(&self, timeout: u32) -> io::Result<Option<ExitStatus>> {
        // SAFETY: the process handle is owned and valid for this entire call.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), timeout) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: valid owned process handle and writable exit-code storage.
                check(unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) })?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        // SAFETY: the handle is owned and refers to the process we created.
        check(unsafe { TerminateProcess(self.handle.as_raw_handle(), 1) })
    }
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut text: Vec<_> = value.encode_wide().collect();
    if text.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "interior NUL"));
    }
    text.push(0);
    Ok(text)
}

fn duplicate(handle: BorrowedHandle<'_>) -> io::Result<OwnedHandle> {
    let mut copy = null_mut();
    // SAFETY: source is borrowed for the call; the pseudo process handle is
    // valid. The returned duplicate is independently owned and inheritable.
    unsafe {
        let process = GetCurrentProcess();
        check(DuplicateHandle(
            process,
            handle.as_raw_handle(),
            process,
            &mut copy,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ))?;
        Ok(OwnedHandle::from_raw_handle(copy))
    }
}

struct Attributes {
    // Pointer-aligned storage; Vec's allocation stays fixed for this lifetime.
    _storage: Vec<usize>,
    pointer: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: the first call queries size only and does not dereference a list.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let pointer = storage.as_mut_ptr().cast();
        // SAFETY: storage is aligned and has at least the queried number of bytes.
        check(unsafe { InitializeProcThreadAttributeList(pointer, 1, 0, &mut bytes) })?;
        Ok(Self {
            _storage: storage,
            pointer,
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialization succeeded and the backing allocation is still live.
        unsafe { DeleteProcThreadAttributeList(self.pointer) };
    }
}

/// Spawns a detached executable, inheriting ONLY these three stdio handles.
///
/// The command is an argument-vector command, not a shell/raw command line.
/// Stdio is supplied here, not taken from Command; the child is created with
/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`. The engine builds this
/// command with args, cwd, and environment overrides only: env_clear and
/// Windows raw_arg/creation_flags are deliberately not part of this interface.
/// Intended for detached ProcessSpec only; foreground helpers still use std.
pub fn spawn(command: &Command, stdio: [BorrowedHandle<'_>; 3]) -> io::Result<Child> {
    create_child(command, stdio, DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
}

/// A child started by [`spawn_piped`]: the process itself plus the read ends
/// of its standard output and error pipes.
pub struct PipedChild {
    pub child: Child,
    pub stdout: File,
    pub stderr: File,
}

/// Spawns a long-lived child whose output this process streams, inheriting
/// ONLY the handles this call creates for it.
///
/// The engine reads a running service's output through pipes. The service
/// outlives the interface that started it, so with an unrestricted
/// CreateProcess it would carry copies of that interface's own inheritable
/// handles - for example the pipe a capturing caller reads - and keep them
/// open until the service stops. The handle allowlist forbids that: the child
/// receives exactly its NUL standard input and the two pipe write ends.
/// `creation_flags` must not add a console or stdio of its own.
pub fn spawn_piped(command: &Command, creation_flags: u32) -> io::Result<PipedChild> {
    let (stdout, stdout_write) = anonymous_pipe()?;
    let (stderr, stderr_write) = anonymous_pipe()?;
    let stdin = File::options().read(true).open(r"\\.\NUL")?;
    let child = create_child(
        command,
        [
            stdin.as_handle(),
            stdout_write.as_handle(),
            stderr_write.as_handle(),
        ],
        creation_flags,
    )?;
    // The child received its own copies through the allowlist; closing the
    // launcher's child-side ends here leaves only the read ends to stream.
    drop(stdin);
    drop(stdout_write);
    drop(stderr_write);
    Ok(PipedChild {
        child,
        stdout,
        stderr,
    })
}

/// Creates an anonymous pipe whose ends carry no inheritability flag; the
/// handle list, not handle flags, decides what a child receives.
fn anonymous_pipe() -> io::Result<(File, OwnedHandle)> {
    let mut read = null_mut();
    let mut write = null_mut();
    // SAFETY: writable handle storage; null attributes give default (private,
    // non-inheritable) handles.
    check(unsafe { CreatePipe(&mut read, &mut write, null(), 0) })?;
    // SAFETY: CreatePipe succeeded; both handles are owned from here on.
    let read = unsafe { File::from_raw_handle(read) };
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    Ok((read, write))
}

/// Starts `command` with exactly `stdio` as its three standard handles.
fn create_child(
    command: &Command,
    stdio: [BorrowedHandle<'_>; 3],
    creation_flags: u32,
) -> io::Result<Child> {
    let program = resolve_program(command)?;
    let application = wide(program.as_os_str())?;
    let mut line = Vec::new();
    quote(program.as_os_str(), &mut line)?;
    for arg in command.get_args() {
        line.push(b' ' as u16);
        quote(arg, &mut line)?;
    }
    line.push(0);
    let directory = command
        .get_current_dir()
        .map(|path| wide(path.as_os_str()))
        .transpose()?;
    let environment = environment(command)?;
    let handles = [
        duplicate(stdio[0])?,
        duplicate(stdio[1])?,
        duplicate(stdio[2])?,
    ];
    let mut allowed: [HANDLE; 3] = std::array::from_fn(|i| handles[i].as_raw_handle());
    let attributes = Attributes::new()?;
    // SAFETY: allowed contains three valid, distinct, inheritable handles. Its
    // storage and every handle remain alive through CreateProcessW. Only these
    // handles may be inherited, even if the CLI owns other inheritable handles.
    check(unsafe {
        UpdateProcThreadAttribute(
            attributes.pointer,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            allowed.as_mut_ptr().cast(),
            size_of_val(&allowed),
            null_mut(),
            null_mut(),
        )
    })?;
    // SAFETY: all-zero is a valid starting representation of these Win32 structs.
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = allowed[0];
    startup.StartupInfo.hStdOutput = allowed[1];
    startup.StartupInfo.hStdError = allowed[2];
    startup.lpAttributeList = attributes.pointer;
    // SAFETY: zeroed output storage, filled only on successful CreateProcessW.
    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    // SAFETY: all strings are NUL terminated; line is writable; environment is
    // double-NUL terminated. The startup pointer covers the entire extended
    // structure; the attribute list and its values outlive the call.
    // TRUE is REQUIRED by HANDLE_LIST, but the list restricts inheritance.
    // Null security attributes make the returned process/thread handles private.
    check(unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            creation_flags | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            directory.as_ref().map_or(null(), |s| s.as_ptr()),
            (&startup as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
            &mut info,
        )
    })?;
    // SAFETY: successful CreateProcessW transfers ownership of these two handles.
    let handle = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    // We need no thread operations, so close its handle immediately.
    drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
    Ok(Child {
        handle,
        pid: info.dwProcessId,
    })
}

// CRT/Rust argument quoting: double backslashes before quotes and before the
// closing quote. Always quote, including empty arguments. No shell is involved.
fn quote(arg: &OsStr, line: &mut Vec<u16>) -> io::Result<()> {
    let text = wide(arg)?;
    line.push(34);
    let mut slashes = 0;
    for &unit in &text[..text.len() - 1] {
        if unit == 92 {
            slashes += 1;
        } else {
            let count = if unit == 34 { slashes * 2 + 1 } else { slashes };
            line.extend(std::iter::repeat_n(92, count));
            line.push(unit);
            slashes = 0;
        }
    }
    line.extend(std::iter::repeat_n(92, slashes * 2));
    line.push(34);
    Ok(())
}

fn key_cmp(a: &OsStr, b: &OsStr) -> Ordering {
    let a: Vec<_> = a.encode_wide().collect();
    let b: Vec<_> = b.encode_wide().collect();
    let (a_len, b_len) = match (i32::try_from(a.len()), i32::try_from(b.len())) {
        (Ok(a_len), Ok(b_len)) => (a_len, b_len),
        _ => return a.cmp(&b),
    };
    // SAFETY: both buffers remain live and the checked lengths cannot become
    // negative (which would ask Windows to scan for an absent NUL terminator).
    match unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) } {
        1 => Ordering::Less,
        2 => Ordering::Equal,
        3 => Ordering::Greater,
        _ => a.cmp(&b),
    }
}

fn environment(command: &Command) -> io::Result<Vec<u16>> {
    let mut entries: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (key, value) in command.get_envs() {
        if key.is_empty() || key.encode_wide().any(|c| c == 0 || c == 61) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid environment key",
            ));
        }
        entries.retain(|(old, _)| key_cmp(old, key) != Ordering::Equal);
        if let Some(value) = value {
            entries.push((key.to_owned(), value.to_owned()));
        }
    }
    entries.sort_by(|a, b| key_cmp(&a.0, &b.0));
    let mut block = Vec::new();
    for (key, value) in entries {
        let key = wide(&key)?;
        block.extend_from_slice(&key[..key.len() - 1]);
        block.push(61);
        block.extend(wide(&value)?);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn resolve_program(command: &Command) -> io::Result<PathBuf> {
    let program = Path::new(command.get_program());
    let mut candidate = program.to_path_buf();
    if candidate.extension().is_none() {
        candidate.set_extension("exe");
    }
    // Native executable launching only: do not introduce cmd.exe interpretation.
    if candidate.extension().is_some_and(|s| {
        s.as_encoded_bytes().eq_ignore_ascii_case(b"bat")
            || s.as_encoded_bytes().eq_ignore_ascii_case(b"cmd")
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "detached services must be native executables",
        ));
    }
    if candidate.is_absolute() || candidate.components().count() > 1 {
        return std::fs::canonicalize(candidate);
    }
    let path = command
        .get_envs()
        .find(|(k, _)| key_cmp(k, OsStr::new("PATH")) == Ordering::Equal)
        .map(|(_, v)| v.map(OsStr::to_owned))
        .unwrap_or_else(|| std::env::var_os("PATH"));
    if let Some(path) = path {
        for directory in std::env::split_paths(&path).filter(|p| !p.as_os_str().is_empty()) {
            let file = directory.join(&candidate);
            if file.is_file() {
                return std::fs::canonicalize(file);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("executable not found: {}", program.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_empty_spaces_quotes_and_trailing_backslashes() {
        for (input, expected) in [
            ("", "\"\""),
            ("plain", "\"plain\""),
            ("two words", "\"two words\""),
            ("a\"b", "\"a\\\"b\""),
            ("C:\\with space\\", "\"C:\\with space\\\\\""),
        ] {
            let mut line = Vec::new();
            quote(OsStr::new(input), &mut line).unwrap();
            assert_eq!(String::from_utf16(&line).unwrap(), expected);
        }
        assert!(quote(OsStr::new("bad\0arg"), &mut Vec::new()).is_err());
    }

    #[test]
    fn environment_overrides_are_case_insensitive_and_nul_terminated() {
        let mut command = Command::new("unused.exe");
        command.env("LAMBO_HANDLE_TEST", "first");
        command.env("lambo_handle_test", "second λ");
        let block = environment(&command).unwrap();
        assert!(block.ends_with(&[0, 0]));
        let matching: Vec<_> = block
            .split(|c| *c == 0)
            .filter(|entry| !entry.is_empty())
            .map(String::from_utf16_lossy)
            .filter(|entry| entry.to_ascii_lowercase().starts_with("lambo_handle_test="))
            .collect();
        assert_eq!(matching.len(), 1);
        assert!(matching[0].ends_with("=second λ"));
    }
}
