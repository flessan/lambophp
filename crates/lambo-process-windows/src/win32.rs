//! The Win32 calls that are not process spawning.
//!
//! Three families live here, all of them ported from `pathenv.go`:
//!
//! * the user `PATH`, which Windows keeps in `HKCU\Environment` as a string
//!   (`REG_SZ`) or as an expandable string (`REG_EXPAND_SZ`),
//! * the machine `PATH` in `HKLM`, read so that a tool which shadows one of
//!   Windows' own executables can be left out of the user's `PATH`,
//! * the `Run` key Windows starts programs from at login, one string value per
//!   program, and
//! * elevation: whether this process is elevated, and how to ask Windows for an
//!   elevated copy of it.
//!
//! Everything here is a thin wrapper. The rules - which directories are
//! candidates, which ones are skipped, how the value is joined and whether it
//! stays expandable - are in `lambo_core::pathenv`, where they can be tested
//! without a registry.

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Registry::HKEY;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SW_SHOWNORMAL, SendMessageTimeoutW, WM_SETTINGCHANGE,
};

/// The subkey of `HKCU` that holds the user's environment.
const USER_ENVIRONMENT: &str = "Environment";

/// The subkey of `HKLM` that holds the machine's environment.
const MACHINE_ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// The value both keys hold the `PATH` in.
const PATH_VALUE: &str = "Path";

/// The subkey of `HKCU` that Windows starts programs from at login.
///
/// The path itself is in `lambo_core::tray`, with the value names and the
/// rules; this is only where the calls go.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// A NUL-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An open registry key, closed when it goes out of scope - including on the
/// error paths, so a `?` between opening and returning cannot leak it.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // The only failure `RegCloseKey` can report is an invalid handle, which
        // is not something a caller could act on.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Opens a key for reading, or for reading and writing.
fn open(root: HKEY, subkey: &str, write: bool) -> io::Result<Key> {
    let subkey = wide(subkey);
    let access = if write {
        KEY_QUERY_VALUE | KEY_SET_VALUE
    } else {
        KEY_QUERY_VALUE
    };
    let mut key: HKEY = null_mut();
    let status = unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, access, &mut key) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(Key(key))
}

/// Reads a string value, or `None` when the value is not there.
///
/// Windows stores a `PATH` as UTF-16 with the terminator included in the size,
/// so the terminator is trimmed. A value that exists but is empty is
/// `Some("")`, not `None`: the difference decides which branch of the caller's
/// "no `PATH` yet" handling runs.
fn read_string(key: HKEY, name: &str) -> io::Result<Option<String>> {
    let name = wide(name);

    let mut kind: u32 = 0;
    let mut size: u32 = 0;
    let status =
        unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut kind, null_mut(), &mut size) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if size == 0 {
        return Ok(Some(String::new()));
    }

    // One extra unit of room for a terminator the size may not include.
    let mut buffer = vec![0u16; size as usize / 2 + 1];
    let mut buffer_size = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null(),
            &mut kind,
            buffer.as_mut_ptr().cast::<u8>(),
            &mut buffer_size,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    let length = (buffer_size as usize / 2).min(buffer.len());
    let mut text = String::from_utf16_lossy(&buffer[..length]);
    while text.ends_with('\0') {
        text.pop();
    }
    Ok(Some(text))
}

/// Writes a string value, keeping it expandable when the existing one was.
fn write_string(key: HKEY, name: &str, value: &str, expandable: bool) -> io::Result<()> {
    let name = wide(name);
    let data = wide(value);
    let kind = if expandable { REG_EXPAND_SZ } else { REG_SZ };
    let status = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            kind,
            data.as_ptr().cast::<u8>(),
            (data.len() * 2) as u32,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// The user's `PATH` from `HKCU\Environment`, or `None` when it is not set.
///
/// This is the call behind the original's `open HKCU\Environment: ..` and
/// `read Path: ..` messages.
pub fn user_path() -> io::Result<Option<String>> {
    let key = open(HKEY_CURRENT_USER, USER_ENVIRONMENT, false)?;
    read_string(key.0, PATH_VALUE)
}

/// Replaces the user's `PATH`, keeping it expandable when it holds `%name%`
/// references. This is the call behind the original's `write Path: ..` message.
pub fn set_user_path(value: &str, expandable: bool) -> io::Result<()> {
    let key = open(HKEY_CURRENT_USER, USER_ENVIRONMENT, true)?;
    write_string(key.0, PATH_VALUE, value, expandable)
}

/// The machine's `PATH` from `HKLM`, or `None` when it cannot be read.
///
/// A failure is not an error here: the caller only uses this to *avoid*
/// shadowing a system tool, so an unreadable machine `PATH` means nothing is
/// skipped - which is what the original did with a nil map.
pub fn machine_path() -> Option<String> {
    let key = open(HKEY_LOCAL_MACHINE, MACHINE_ENVIRONMENT, false).ok()?;
    read_string(key.0, PATH_VALUE).ok().flatten()
}

/// Tells every top-level window that the environment changed.
///
/// Without this a shell that is already running keeps the `PATH` it read at
/// logon, and a newly installed tool stays invisible until the next sign-in.
/// The result is ignored on purpose: the value has been written, and a window
/// that does not answer within the timeout must not fail the install. The
/// 1000 ms timeout is the original's.
pub fn broadcast_environment_change() {
    const TIMEOUT_MS: u32 = 1000;

    let environment = wide("Environment");
    let mut result: usize = 0;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            TIMEOUT_MS,
            &mut result,
        );
    }
}

/// Whether this process holds an elevated token.
///
/// A token that cannot be inspected is reported as "not elevated", which is
/// what the original did: the caller only uses this to decide whether to offer
/// the button that asks for elevation.
pub fn is_elevated() -> bool {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast::<c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Starts `exe` again through the UAC prompt, which is what the Settings page's
/// "Restart as Admin" button does.
///
/// `ShellExecuteW` reports failure as a code of 32 or less rather than through
/// `GetLastError`, so that value *is* the error - the same value the original
/// turned
/// into an `Errno` (1223, "the operation was canceled by the user", when the
/// prompt is dismissed).
pub fn run_elevated(exe: &Path) -> io::Result<()> {
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

/// A string value of the user's `Run` key, or `None` when it is not set.
///
/// The key itself is created by Windows, so a missing key means "nothing is set
/// to start at login" rather than a failure - which is why this reports the
/// absence instead of an error.
pub fn run_value(name: &str) -> io::Result<Option<String>> {
    let key = match open(HKEY_CURRENT_USER, RUN_KEY, false) {
        Ok(key) => key,
        // `RegOpenKeyExW` reports a key that does not exist as "file not
        // found", and a user who has never set a startup program has no key.
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => return Ok(None),
        Err(error) => return Err(error),
    };
    read_string(key.0, name)
}

/// Sets a `Run` value, creating the key when it is not there yet.
pub fn set_run_value(name: &str, command: &str) -> io::Result<()> {
    let key = open(HKEY_CURRENT_USER, RUN_KEY, true)?;
    write_string(key.0, name, command, false)
}

/// Removes a `Run` value, reporting whether it was there.
pub fn delete_run_value(name: &str) -> io::Result<bool> {
    let key = match open(HKEY_CURRENT_USER, RUN_KEY, true) {
        Ok(key) => key,
        Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };

    let name = wide(name);
    let status = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
    match status {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        _ => Err(io::Error::from_raw_os_error(status as i32)),
    }
}
