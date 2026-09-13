//! Lambo PHP - the desktop application.
//!
//! A thin view over [`lambo_core::app`]. Every action here calls the same
//! application API the CLI calls; nothing in this crate decides anything about
//! ports, runtimes or services. That is deliberate: a GUI that reimplements the
//! engine's decisions would eventually disagree with it, and the user would see
//! two answers to the same question depending on which front end they used.
//!
//! # Layout
//!
//! - [`view`] - what to show. Platform-independent and unit-tested everywhere.
//! - `win32` - how to draw it. Windows-only, and the only unverifiable part.
//!
//! Almost everything that could be *wrong* about the dashboard is a decision
//! about text, not a Win32 call, so it lives in [`view`] where CI can check it.
//!
//! # Unsafe code
//!
//! The workspace forbids `unsafe` and this crate relaxes that in its manifest,
//! because Win32 is FFI and a native window cannot be built without it. The
//! relaxation is scoped to this crate only - the engine and the CLI keep the
//! prohibition - and within it, `view` re-forbids it so the tested half cannot
//! drift into FFI.

pub mod view;

#[cfg(windows)]
mod win32;

fn main() {
    #[cfg(windows)]
    {
        // The window owns the process; it returns when the message loop ends.
        let code = win32::run();
        std::process::exit(code);
    }

    #[cfg(not(windows))]
    {
        // Not a stub that pretends to work. The GUI is a Win32 application
        // because Windows is the primary target; saying so plainly is better
        // than a window that silently cannot do what the Windows one does.
        eprintln!("lambo-gui is a Windows application.");
        eprintln!();
        eprintln!("On this platform use the CLI, which offers the same engine:");
        eprintln!("  lambo up            start the project");
        eprintln!("  lambo status        what is running");
        eprintln!("  lambo open          open http://localhost");
        eprintln!("  lambo db open       open the database manager");
        eprintln!("  lambo down          stop everything Lambo started");
        std::process::exit(1);
    }
}
