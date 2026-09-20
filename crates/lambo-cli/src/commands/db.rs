//! `lambo db` - the database, end to end.
//!
//! Lambo owns the server, its data directory and its credentials, so every
//! command here works without the user installing anything or knowing a
//! password: it is generated once, stored in Lambo's own configuration, and
//! passed to clients through the environment rather than the command line.

use std::process::ExitCode;

use clap::Subcommand;
use lambo_core::config::DatabaseKind;
use lambo_core::database::{self, Credentials};
use lambo_core::dbui;
use lambo_core::process;
use lambo_core::session;
use lambo_core::session::DATABASE_SERVICES;
use lambo_core::version::VersionSpec;

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Download and install the database server.
    Install {
        /// Engine to install [default: the configured `database.kind`].
        #[arg(long)]
        engine: Option<String>,

        /// Version to install [default: the newest stable release].
        #[arg(long)]
        version: Option<String>,
    },

    /// Download and install the browser database manager (Adminer).
    InstallUi,

    /// Start the database server.
    Start,

    /// Stop the database server.
    Stop,

    /// Restart the database server.
    Restart,

    /// Show whether the database server is running.
    Status,

    /// Create a database.
    Create {
        /// Name to create [default: the current project's database].
        name: Option<String>,
    },

    /// Drop a database.
    Drop {
        /// Name to drop.
        name: String,

        /// Do not ask for confirmation.
        #[arg(long)]
        force: bool,
    },

    /// List the databases on the server.
    List,

    /// Open an interactive SQL shell.
    Shell {
        /// Database to connect to [default: the current project's database].
        name: Option<String>,
    },

    /// Open the browser database manager.
    Open,

    /// Print the connection details of this project's database.
    Credentials {
        /// Print the password too.
        #[arg(long = "show-password")]
        show_password: bool,
    },
}

pub fn run(ui: &Ui, command: DbCommand) -> Result<ExitCode> {
    let mut context = super::context()?;

    match command {
        DbCommand::Install { engine, version } => install(ui, &mut context, engine, version),
        DbCommand::InstallUi => install_ui(ui, &mut context),
        DbCommand::Start => start(ui, &mut context),
        DbCommand::Stop => stop(ui, &mut context),
        DbCommand::Restart => {
            stop(ui, &mut context)?;
            start(ui, &mut context)
        }
        DbCommand::Status => status(ui, &mut context),
        DbCommand::Create { name } => create(ui, &mut context, name),
        DbCommand::Drop { name, force } => drop_database(ui, &mut context, &name, force),
        DbCommand::List => list(ui, &mut context),
        DbCommand::Shell { name } => shell(ui, &mut context, name),
        DbCommand::Open => open(ui, &mut context),
        DbCommand::Credentials { show_password } => credentials(ui, &mut context, show_password),
    }
}

fn install(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    engine: Option<String>,
    version: Option<String>,
) -> Result<ExitCode> {
    context.paths.ensure_layout()?;
    let kind = match engine {
        Some(engine) => engine.parse::<DatabaseKind>()?,
        None => context.config.database.kind,
    };
    if !kind.is_enabled() {
        return Err(lambo_core::Error::InvalidInput(
            "no engine was given and `database.kind` is `none`; \
             pass --engine mariadb or --engine mysql"
                .to_owned(),
        )
        .into());
    }
    let spec = match version {
        Some(version) => version.parse::<VersionSpec>()?,
        None => VersionSpec::Stable,
    };

    ui.section(&format!("Installing {}", kind.display_name()));
    let database = database::install(
        &context.paths,
        &context.catalog,
        kind,
        &spec,
        context.platform,
        context.downloader,
        &context.config.sources,
    )?;

    ui.ok(format!("installed {}", database.describe()));
    ui.kv("server", database.server.display());
    ui.hint("`lambo db start` initializes the data directory and starts it");
    Ok(ExitCode::SUCCESS)
}

fn install_ui(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    context.paths.ensure_layout()?;
    let installed = dbui::install(
        &context.paths,
        &context.catalog,
        &VersionSpec::Stable,
        context.platform,
        context.downloader,
        &context.config.sources,
    )?;
    ui.ok(format!(
        "installed {} at {}",
        context.config.dbui.kind,
        installed.entry.display()
    ));
    ui.hint("`lambo db open` starts it and opens your browser");
    Ok(ExitCode::SUCCESS)
}

fn start(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    context.paths.ensure_layout()?;
    let project = super::project_or_none()?;
    let (kind, port) = wanted(context, project.as_ref());
    if !kind.is_enabled() {
        return Err(not_enabled().into());
    }
    ui.ok(session::start_database(
        context,
        project.as_ref(),
        kind,
        port,
    )?);
    Ok(ExitCode::SUCCESS)
}

fn stop(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    ui.ok(session::stop_database(context)?);
    Ok(ExitCode::SUCCESS)
}

fn status(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let status = session::status(None, context)?;
    ui.section("Database");
    let Some(service) = status
        .services
        .iter()
        .find(|service| DATABASE_SERVICES.contains(&service.name.as_str()))
    else {
        ui.kv("state", "not started");
        return Ok(ExitCode::SUCCESS);
    };
    let line = service.state_word();
    if service.running {
        ui.ok(line);
    } else {
        ui.bullet(line);
    }
    if let Some(pid) = service.pid.filter(|_| service.running) {
        ui.kv("pid", pid);
    }
    if let Some(port) = service.port {
        ui.kv("port", port);
    }
    if let Some(uptime) = &service.uptime {
        ui.kv("uptime", uptime);
    }
    if let Some(log) = &service.log {
        ui.kv("log", log.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn create(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    name: Option<String>,
) -> Result<ExitCode> {
    let project = super::project_or_none()?;
    let (database, plan) = target(context, project.as_ref())?;
    let name = name.unwrap_or_else(|| match &project {
        Some(project) => project.database_name(),
        None => String::new(),
    });
    if name.is_empty() {
        return Err(lambo_core::Error::InvalidInput(
            "no database name was given and this directory is not a Lambo project".to_owned(),
        )
        .into());
    }
    database::create_database(&database, &plan, &name, context.os)?;
    ui.ok(format!("created database `{name}`"));
    Ok(ExitCode::SUCCESS)
}

fn drop_database(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    name: &str,
    force: bool,
) -> Result<ExitCode> {
    if !force {
        ui.warn(format!(
            "this deletes the database `{name}` and everything in it"
        ));
        ui.hint("re-run with --force to confirm");
        return Ok(ExitCode::FAILURE);
    }
    let project = super::project_or_none()?;
    let (database, plan) = target(context, project.as_ref())?;
    database::drop_database(&database, &plan, name, context.os)?;
    ui.ok(format!("dropped database `{name}`"));
    Ok(ExitCode::SUCCESS)
}

fn list(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let project = super::project_or_none()?;
    let (database, plan) = target(context, project.as_ref())?;
    let names = database::list_databases(&database, &plan, context.os)?;
    if names.is_empty() {
        ui.kv("databases", "none");
        return Ok(ExitCode::SUCCESS);
    }
    for name in names {
        ui.plain(name);
    }
    Ok(ExitCode::SUCCESS)
}

fn shell(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    name: Option<String>,
) -> Result<ExitCode> {
    let project = super::project_or_none()?;
    let (database, plan) = target(context, project.as_ref())?;
    let name = name.or_else(|| project.as_ref().map(|project| project.database_name()));
    let Some(spec) = database::shell_spec(&database, &plan, name.as_deref()) else {
        return Err(lambo_core::Error::RuntimeNotInstalled {
            kind: "a database client",
            name: database.describe(),
            path: database.server.clone(),
        }
        .into());
    };

    // Interactive: the shell owns the terminal, and Lambo waits for it.
    let mut child = process::spawn(&spec, context.os)?;
    let status = child.wait().map_err(|source| lambo_core::Error::Io {
        path: database
            .client
            .clone()
            .unwrap_or_else(|| database.server.clone()),
        source,
    })?;
    let _ = ui;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
        None => ExitCode::FAILURE,
    })
}

fn open(ui: &Ui, context: &mut lambo_core::session::Context<'_>) -> Result<ExitCode> {
    context.paths.ensure_layout()?;
    let project = super::project_or_none()?;
    let name = project.as_ref().map(|project| project.database_name());
    let url = session::open_database_ui(context, name.as_deref(), true)?;
    ui.ok(format!("opened {url}"));
    ui.hint("the password is not in the URL; `lambo db credentials --show-password` prints it");
    Ok(ExitCode::SUCCESS)
}

fn credentials(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    show_password: bool,
) -> Result<ExitCode> {
    let project = super::project_or_none()?;
    let (database, plan) = target(context, project.as_ref())?;
    let name = project.as_ref().map(|project| project.database_name());
    let kind = match &project {
        Some(project) => project.database_kind(&context.config),
        None => context.config.database.kind,
    };
    let _ = database;
    ui.plain(Credentials::for_plan(&plan, kind, name.as_deref()).render(show_password));
    Ok(ExitCode::SUCCESS)
}

/// The engine and port this command should act on.
fn wanted(
    context: &lambo_core::session::Context<'_>,
    project: Option<&lambo_core::project::Project>,
) -> (DatabaseKind, u16) {
    match project {
        Some(project) => (
            project.database_kind(&context.config),
            project.file.database_port(&context.config),
        ),
        None => (context.config.database.kind, context.config.database.port),
    }
}

/// Resolves the server and plan for a command that talks to the database.
fn target(
    context: &mut lambo_core::session::Context<'_>,
    project: Option<&lambo_core::project::Project>,
) -> Result<(database::Database, database::Plan)> {
    let (kind, port) = wanted(context, project);
    if !kind.is_enabled() {
        return Err(not_enabled().into());
    }
    Ok(session::database_target(context, kind, port)?)
}

fn not_enabled() -> lambo_core::Error {
    lambo_core::Error::InvalidInput(
        "no database engine is configured; set `database.kind` in lambo.yml or run \
         `lambo config set database.kind mariadb`"
            .to_owned(),
    )
}
