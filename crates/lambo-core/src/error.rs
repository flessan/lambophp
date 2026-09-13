//! The unified error type.
//!
//! Every failure the core engine can produce is one [`Error`], carrying
//! enough context to print an actionable message: which file, which port,
//! which service, and what to try next. Interfaces (CLI, TUI, GUI) render
//! the same values, so a message written once reads the same everywhere.
//!
//! Errors are deliberately *descriptive* rather than opaque: a user facing
//! `lambo up` at 2 a.m. must be able to tell from the message alone whether
//! the port is taken, the runtime is missing, or a download was rejected.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// The result type used across lambo-core.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong inside the engine.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("{}", describe_io(path, source))]
    Io {
        /// The path the operation targeted.
        path: PathBuf,
        /// The underlying OS error.
        source: std::io::Error,
    },

    /// A YAML document could not be parsed.
    #[error("invalid YAML in `{}`: {source}", path.display())]
    Yaml {
        /// The malformed document.
        path: PathBuf,
        /// Parser diagnostics.
        source: serde_yaml::Error,
    },

    /// A JSON document could not be parsed (e.g. `composer.json`).
    #[error("invalid JSON in `{}`: {source}", path.display())]
    Json {
        /// The malformed document.
        path: PathBuf,
        /// Parser diagnostics.
        source: serde_json::Error,
    },

    /// Serialization to YAML failed while writing a document.
    #[error("failed to serialize configuration: {0}")]
    Serialize(serde_yaml::Error),

    /// The home directory could not be determined and `LAMBO_HOME` is unset.
    #[error("could not determine the home directory; set the LAMBO_HOME environment variable")]
    NoHomeDir,

    /// A version specification string could not be parsed.
    #[error("invalid version specification `{input}`: {reason}")]
    InvalidVersionSpec {
        /// The offending input.
        input: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A configuration key is not known to `lambo config get/set`.
    #[error("unknown configuration key `{0}`")]
    UnknownConfigKey(String),

    /// A configuration value failed validation.
    #[error("invalid value `{value}` for `{key}`: {reason}")]
    InvalidConfigValue {
        /// The configuration key.
        key: String,
        /// The rejected value.
        value: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A runtime marker references a version that is not installed.
    #[error("{kind} runtime `{name}` is marked active but missing at `{}`", path.display())]
    RuntimeNotInstalled {
        /// Runtime family display name (e.g. `PHP`).
        kind: &'static str,
        /// The referenced version directory name.
        name: String,
        /// Where the runtime was expected.
        path: PathBuf,
    },

    /// A named workspace does not exist.
    #[error("workspace `{0}` does not exist")]
    WorkspaceNotFound(String),

    /// A `lambo.yml` file failed validation.
    #[error("invalid project configuration `{}`: {reason}", path.display())]
    InvalidProjectFile {
        /// The offending project file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },

    /// The command needs a project but none was found.
    #[error("no Lambo project found at or above `{}`", dir.display())]
    NotAProject {
        /// Where the search started.
        dir: PathBuf,
    },

    /// A required runtime (PHP, Apache, MariaDB, …) is not installed.
    #[error("{kind} is not installed; run `{command}`")]
    RuntimeMissing {
        /// Runtime family display name (e.g. `Apache`).
        kind: &'static str,
        /// The command that installs it.
        command: &'static str,
    },

    /// An artifact cannot be installed because Lambo has no way to verify it.
    ///
    /// This is deliberately not a [`Error::Download`] failure: nothing was
    /// fetched, nothing was wrong with the network, and no digest was wrong.
    /// The catalogue simply does not say what the artifact is supposed to be.
    /// Keeping it a distinct variant lets a caller attach a family-specific
    /// remedy instead of pattern-matching on prose.
    #[error(
        "cannot verify `{url}`: {subject} \
         Lambo never downloads, unpacks or runs an artifact whose checksum it \
         cannot confirm, so nothing was fetched.{remedy}",
        subject = subject(family, version, platform),
        remedy = match hint {
            Some(hint) => format!(" {hint}"),
            None => String::new(),
        },
    )]
    VerificationUnavailable {
        /// The URL that would have been requested.
        url: String,
        /// Runtime family, when the caller knows it.
        family: Option<String>,
        /// Release version, when the caller knows it.
        version: Option<String>,
        /// Platform key, when the caller knows it.
        platform: Option<String>,
        /// A family-specific way out, when the caller knows one.
        hint: Option<String>,
    },

    /// A download failed.
    #[error("download failed: {url}: {reason}")]
    Download {
        /// The URL that was requested.
        url: String,
        /// What went wrong.
        reason: String,
    },

    /// A download was rejected because it did not match its checksum.
    #[error(
        "checksum verification failed for `{}`: expected {expected}, got {actual} - \
         the file was not extracted or executed",
        path.display()
    )]
    ChecksumMismatch {
        /// The downloaded file.
        path: PathBuf,
        /// The expected SHA-256 hex digest.
        expected: String,
        /// The digest actually computed.
        actual: String,
    },

    /// A download was rejected because it was not served over HTTPS.
    #[error("refusing to download `{0}`: only https:// URLs are allowed")]
    NotHttps(String),

    /// An archive could not be unpacked.
    #[error("could not unpack `{}`: {reason}", path.display())]
    Archive {
        /// The archive.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },

    /// An archive entry tried to escape the destination directory.
    #[error(
        "refusing to unpack `{entry}` from `{}`: entry points outside the destination directory",
        archive.display()
    )]
    UnsafeArchiveEntry {
        /// The archive containing the entry.
        archive: PathBuf,
        /// The offending entry name.
        entry: String,
    },

    /// A port is already in use.
    #[error("port {port} is already in use{}", occupant_hint(occupied_by))]
    PortInUse {
        /// The requested port.
        port: u16,
        /// The process holding it, when it could be determined.
        occupied_by: Option<String>,
    },

    /// A supervised process exited when it should be running.
    #[error("{name} exited {}", exit_reason(*code))]
    ProcessExited {
        /// Human-readable service name.
        name: String,
        /// Exit status, when the process reported one.
        code: Option<i32>,
    },

    /// A service did not become healthy in time.
    #[error("{service} did not become healthy within {seconds}s")]
    Timeout {
        /// The service that timed out.
        service: String,
        /// How long we waited.
        seconds: u64,
    },

    /// An HTTP request failed.
    #[error("HTTP request to {url} failed: {reason}")]
    Http {
        /// The requested URL.
        url: String,
        /// What went wrong.
        reason: String,
    },

    /// A service failed to start; carries the diagnosis shown to the user.
    #[error("{service} failed to start: {reason}")]
    ServiceFailed {
        /// The service that failed.
        service: String,
        /// The primary reason.
        reason: String,
        /// Other likely causes, shown as a checklist.
        causes: Vec<String>,
        /// The command that diagnoses the environment.
        hint: Option<String>,
    },

    /// A database operation failed.
    #[error("database operation failed: {reason}")]
    Database {
        /// What went wrong.
        reason: String,
        /// Where the details are.
        log: Option<PathBuf>,
    },

    /// Something cannot be done on this platform.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),

    /// A user-supplied argument is invalid.
    #[error("{0}")]
    InvalidInput(String),
}

impl Error {
    /// Attaches the catalogue identity to a verification failure.
    ///
    /// [`crate::download::download_verified`] only knows a URL. The family,
    /// version and platform come from the release the caller selected, and
    /// they are what turns this message into an actionable one.
    ///
    /// The family is passed separately: a [`crate::catalog::Release`] does not
    /// record which family it belongs to, because that is decided by which
    /// section of the catalogue it was listed under.
    pub fn identify(
        self,
        family: crate::catalog::Family,
        release: &crate::catalog::Release,
    ) -> Self {
        match self {
            Self::VerificationUnavailable {
                url,
                family: None,
                version: None,
                platform: None,
                hint,
            } => Self::VerificationUnavailable {
                url,
                family: Some(family.to_string()),
                version: Some(release.version.clone()),
                platform: Some(release.platform.clone()),
                hint,
            },
            other => other,
        }
    }

    /// Attaches a remedy to a verification failure, leaving other errors alone.
    pub fn with_hint(self, hint: impl Into<String>) -> Self {
        match self {
            Self::VerificationUnavailable {
                url,
                family,
                version,
                platform,
                ..
            } => Self::VerificationUnavailable {
                url,
                family,
                version,
                platform,
                hint: Some(hint.into()),
            },
            other => other,
        }
    }
}

/// Names the artifact a [`Error::VerificationUnavailable`] refers to.
fn subject(family: &Option<String>, version: &Option<String>, platform: &Option<String>) -> String {
    match (family, version, platform) {
        (Some(family), Some(version), Some(platform)) => {
            format!("{family} {version} for {platform} has no pinned digest in the catalogue.")
        }
        _ => "the catalogue has no pinned digest for it.".to_owned(),
    }
}

impl Error {
    /// Builds an [`Error::Io`] with attached path context.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Builds an [`Error::ServiceFailed`] from a reason and candidate causes.
    pub fn service_failed(
        service: impl Into<String>,
        reason: impl Into<String>,
        causes: impl Into<Vec<String>>,
    ) -> Self {
        Self::ServiceFailed {
            service: service.into(),
            reason: reason.into(),
            causes: causes.into(),
            hint: Some("lambo doctor".to_owned()),
        }
    }

    /// The extra lines that make an error actionable.
    ///
    /// [`Display`](std::fmt::Display) for an error is one line, which is what a
    /// log needs. A user staring at a terminal needs more: the other things
    /// that could be wrong, and the command to run next. This is the single
    /// place that knowledge lives, so the CLI, a future GUI and the tests all
    /// agree on what the user is told.
    /// The command that fixes this, when one is known.
    ///
    /// Exposed separately from [`details`](Self::details) because a GUI renders
    /// a hint as an actionable button rather than as a line of prose. Parsing it
    /// back out of `details` would be exactly the fragile coupling this
    /// accessor exists to avoid.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::ServiceFailed { hint, .. } => hint.clone(),
            Self::RuntimeMissing { command, .. } => Some((*command).to_owned()),
            Self::RuntimeNotInstalled { .. }
            | Self::ChecksumMismatch { .. }
            | Self::VerificationUnavailable { .. } => Some("lambo doctor".to_owned()),
            _ => None,
        }
    }

    pub fn details(&self) -> Vec<String> {
        let mut details = Vec::new();
        match self {
            Self::ServiceFailed { causes, hint, .. } => {
                if !causes.is_empty() {
                    details.push("possible causes:".to_owned());
                    details.extend(causes.iter().map(|cause| format!("  - {cause}")));
                }
                if let Some(hint) = hint {
                    details.push(format!("next: {hint}"));
                }
            }
            Self::RuntimeMissing { command, .. } => details.push(format!("next: {command}")),
            Self::RuntimeNotInstalled { path, .. } => {
                details.push(format!("expected at: {}", path.display()));
                details.push("next: lambo doctor".to_owned());
            }
            Self::Database { log: Some(log), .. } => {
                details.push(format!("details: {}", log.display()));
            }
            Self::ChecksumMismatch { .. } => {
                details.push("nothing was extracted or executed".to_owned());
                details.push("next: lambo doctor".to_owned());
            }
            Self::UnsafeArchiveEntry { archive, .. } => {
                details.push(format!("archive: {}", archive.display()));
                details.push("the download was rejected and nothing was written".to_owned());
            }
            Self::NotAProject { dir } => {
                details.push(format!("searched from: {}", dir.display()));
                details.push("next: lambo init".to_owned());
            }
            Self::Timeout { service, .. } => {
                details.push(format!("{service} never answered"));
                details.push("next: lambo logs".to_owned());
            }
            _ => {}
        }
        details
    }
}

/// Formats a path-aware I/O message: `failed to read C:\Lambo\config: …`.
fn describe_io(path: &Path, source: &std::io::Error) -> String {
    format!("failed to access `{}`: {source}", path.display())
}

/// Formats the exit status of a process for humans.
fn exit_reason(code: Option<i32>) -> String {
    match code {
        Some(0) => "unexpectedly with status 0".to_owned(),
        Some(code) => format!("with status {code}"),
        None => "without reporting a status (killed?)".to_owned(),
    }
}

/// Formats the "…by <process>" suffix of a port conflict message.
fn occupant_hint(occupied_by: &Option<String>) -> String {
    match occupied_by {
        Some(name) => format!(" by {name}"),
        None => String::new(),
    }
}
