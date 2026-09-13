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
    match cli::run() {
        Ok(code) => code,
        Err(err) => {
            ui::report_error(&err);
            err.exit_code()
        }
    }
}
