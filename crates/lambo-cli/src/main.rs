//! The `lambo` command-line interface.
//!
//! Everything here is a thin shell over `lambo-core`: this crate parses
//! arguments, renders output, and delegates every decision to the core
//! engine. Keeping zero business logic here is what lets a GUI reuse 100% of
//! the behaviour later - see docs/architecture.md.

#![forbid(unsafe_code)]

mod cli;
mod commands;
mod error;
mod ui;

use std::process::ExitCode;

fn main() -> ExitCode {
    // `lambo --hide-run <program> [args...]` is how a detached server is
    // started from a hidden window: it is not a command, so it never reaches the
    // parser. The exit status is 0 whatever happens, as the original's was - the
    // launcher reads the PID file and reports the failure from there.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = lambo_core::service::hide_run_from_argv(&argv) {
        if let Err(err) = result {
            ui::report_error(&err.into());
        }
        return ExitCode::SUCCESS;
    }

    match cli::run() {
        Ok(code) => code,
        Err(err) => {
            ui::report_error(&err);
            err.exit_code()
        }
    }
}
