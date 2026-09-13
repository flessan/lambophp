//! CLI-level errors: presentation-shaped wrappers over
//! [`lambo_core::Error`].

use std::process::ExitCode;

use thiserror::Error;

/// Every failure a command can produce.
#[derive(Debug, Error)]
pub enum CliError {
    /// A failure bubbled up from the core engine.
    #[error(transparent)]
    Core(#[from] lambo_core::Error),

    /// The configured editor could not be started.
    #[error("failed to launch editor `{program}`: {source}")]
    Editor {
        /// The editor command that was attempted.
        program: String,
        /// Why spawning failed.
        source: std::io::Error,
    },
}

impl CliError {
    /// Process exit code for this failure.
    ///
    /// Every failure is fatal for the command that hit it, so the code is
    /// uniform; the *reason* is what differs and it is printed, not encoded.
    pub fn exit_code(&self) -> ExitCode {
        ExitCode::FAILURE
    }
}

/// Result alias used throughout the CLI.
pub type Result<T> = std::result::Result<T, CliError>;
