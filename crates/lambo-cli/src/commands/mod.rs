//! Command implementations.
//!
//! Every command renders through [`crate::ui::Ui`] and delegates business
//! logic to `lambo-core`. If a command grows a rule, a schema, or a
//! condition, that logic belongs in the core engine, not here.

pub mod config;
pub mod db;
pub mod doctor;
pub mod down;
pub mod frameworks;
pub mod init;
pub mod logs;
pub mod migrate;
pub mod open;
pub mod php;
pub mod restart;
pub mod server;
pub mod status;
pub mod up;
pub mod verify;
pub mod vhosts;
pub mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use lambo_core::catalog::Catalog;
use lambo_core::config::Config;
use lambo_core::download::SystemDownloader;
use lambo_core::logs::LogFn;
use lambo_core::paths::Paths;
use lambo_core::platform::{Os, Platform};
use lambo_core::project::Project;
use lambo_core::session::Context;

/// `https://` goes to `curl` on every platform, so behaviour is identical on
/// Windows and on Unix; `file://` is copied directly, which is what makes an
/// offline install from a local artifact work. A `static` gives it the
/// `'static` lifetime the session context wants without leaking or boxing.
static DOWNLOADER: SystemDownloader = SystemDownloader;

/// Resolves the process working directory with path context on failure.
pub(crate) fn current_dir() -> lambo_core::Result<PathBuf> {
    std::env::current_dir().map_err(|e| lambo_core::Error::io(".", e))
}

/// Where the engine narrates what it is doing.
///
/// Installing a component and starting a service both take time, and the engine
/// reports each stage as it reaches it. The CLI shows those lines as they
/// arrive; the step report at the end is the summary of the same run.
fn engine_log() -> LogFn {
    Arc::new(|line: &str| println!("{line}"))
}

/// Resolves the home, configuration and catalogue every command needs.
///
/// Deliberately does *not* create the home directory: read-only commands such
/// as `lambo doctor` must be able to report a broken or missing home instead
/// of quietly creating one.
pub(crate) fn context() -> lambo_core::Result<Context<'static>> {
    let paths = Paths::detect()?;
    let config = Config::load(&paths)?;
    let catalog = Catalog::load(&paths)?;
    Ok(Context {
        paths,
        config,
        catalog,
        platform: Platform::host(),
        downloader: &DOWNLOADER,
        os: Os::host(),
        log: engine_log(),
    })
}

/// The same resolution, for commands that only need the home directory.
pub(crate) fn paths() -> lambo_core::Result<Paths> {
    Paths::detect()
}

/// Loads the project of the current directory.
///
/// Fails with [`lambo_core::Error::NotAProject`] when there is no
/// `lambo.yml` at or above it - the message already tells the user to run
/// `lambo init`.
pub(crate) fn project() -> lambo_core::Result<Project> {
    Project::load(&current_dir()?)
}

/// Loads the project when there is one, for commands that work either way.
pub(crate) fn project_or_none() -> lambo_core::Result<Option<Project>> {
    match Project::load(&current_dir()?) {
        Ok(project) => Ok(Some(project)),
        Err(lambo_core::Error::NotAProject { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Renders the steps a session performed, in the order they ran.
///
/// A skipped step is not a warning - it means the step was not needed - so it
/// is printed plainly. Only something that went wrong is red.
pub(crate) fn print_steps(ui: &crate::ui::Ui, report: &lambo_core::session::Report) {
    for step in &report.steps {
        let line = format!("{}: {}", step.name, step.detail);
        match step.outcome {
            lambo_core::session::StepOutcome::Done => ui.ok(line),
            lambo_core::session::StepOutcome::Skipped => ui.bullet(line),
            lambo_core::session::StepOutcome::Failed => ui.fail(line),
        }
    }
}
