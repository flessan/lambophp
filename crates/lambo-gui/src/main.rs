//! Lambo PHP - the desktop application.
//!
//! The interface is the primary way to use Lambo PHP, and it is a shell: the
//! panel's own state and its routing are in [`state`], what it looks like is in
//! [`view`], the engine call behind each action is in [`ops`], and the window
//! that draws it all is in `win32`. Nothing here decides anything about ports,
//! runtimes or services - that is `lambo-core`'s job, and the CLI asks it the
//! same questions, so the two interfaces cannot disagree.
//!
//! [`state`], [`view`] and `ops` contain no Win32 and are unit-tested everywhere.
//! `win32` is the one part that cannot be exercised where it is written; it is
//! cross-compiled and type-checked instead, which is why it does as little as it
//! can.
//!
//! # Unsafe code
//!
//! The workspace forbids `unsafe` and this crate relaxes that in its manifest,
//! because Win32 is FFI and a native window cannot be built without it. The
//! relaxation is scoped to this crate only - the engine and the CLI keep the
//! prohibition - and within it, `view` re-forbids it so the tested half cannot
//! drift into FFI.

// The panel's modules are compiled everywhere so their tests run everywhere, but
// only a Windows build ever reaches the window. On any other platform the
// dead-code lint would call every item of them unused.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod state;
pub mod view;

#[cfg(windows)]
pub mod ops;
#[cfg(windows)]
mod win32;

use std::process::ExitCode;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `lambo-gui --hide-run <program> [args...]`: the detached half of a service
    // start, launched by the engine so the server gets its own hidden,
    // unelevated session. It runs the program and exits, before any window
    // exists. The exit status is 0 whether or not the program started, which is
    // what the original did.
    if let Some(result) = lambo_core::service::hide_run_from_argv(&argv) {
        if let Err(error) = result {
            eprintln!("lambo: {error}");
        }
        return ExitCode::SUCCESS;
    }

    #[cfg(windows)]
    {
        // `--tray` is what the login launch runs: the window starts hidden and
        // lives in the notification area until it is asked for.
        let hidden = argv
            .iter()
            .any(|argument| argument == lambo_core::tray::TRAY_FLAG);
        ExitCode::from(u8::try_from(win32::run(hidden)).unwrap_or(1))
    }

    #[cfg(not(windows))]
    {
        // Not a stub that pretends to work. The panel is a Win32 application
        // because Windows is the platform it serves; saying so plainly is better
        // than a window that silently cannot do what the Windows one does.
        eprintln!("Lambo PHP's panel is a Windows application.");
        eprintln!();
        eprintln!("On this platform use the CLI, which drives the same engine:");
        eprintln!("  lambo up            start the project");
        eprintln!("  lambo status        what is running");
        eprintln!("  lambo open          open http://localhost");
        eprintln!("  lambo db open       open the database manager");
        eprintln!("  lambo down          stop everything Lambo started");
        ExitCode::FAILURE
    }
}
