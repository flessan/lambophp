//! `lambo php` - manage PHP versions, and run PHP.
//!
//! Lambo owns every PHP it installs and never touches the system `PATH`: the
//! version a command uses is the one Lambo resolved, and `lambo php <args>`
//! runs it with a generated `php.ini` via `PHPRC`.

use std::process::ExitCode;

use clap::Subcommand;
use lambo_core::php;
use lambo_core::runtime::InstalledRuntime;
use lambo_core::version::VersionSpec;

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
#[command(allow_hyphen_values = true)]
pub enum PhpCommand {
    /// List the PHP versions installed under Lambo.
    List,

    /// List the PHP versions available to install.
    ListVersions,

    /// Download, verify and install a PHP version.
    Install {
        /// Version to install [default: the newest stable release].
        version: Option<String>,
    },

    /// Make an installed version the active one.
    Use {
        /// Version to activate, e.g. `8.3` or `8.3.10`.
        version: String,
    },

    /// Show the active PHP version and where it lives.
    Current,

    /// Remove an installed version.
    Remove {
        /// Exact version to remove, e.g. `8.3.10`.
        version: String,
    },

    /// Print the path of the generated php.ini.
    Ini,

    /// List the extensions the active PHP has loaded.
    Modules,

    /// Run the active PHP, e.g. `lambo php run -v`.
    #[command(trailing_var_arg = true)]
    Run {
        /// Arguments for PHP.
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Anything else is passed straight to PHP, e.g. `lambo php -v`.
    ///
    /// `allow_hyphen_values` is what makes the documented `lambo php -v`
    /// work: without it clap reads `-v` as an unknown flag of `lambo php`
    /// instead of as an argument for PHP.
    #[command(external_subcommand)]
    Passthrough(#[arg(allow_hyphen_values = true, trailing_var_arg = true)] Vec<String>),
}

pub fn run(ui: &Ui, command: PhpCommand) -> Result<ExitCode> {
    let mut context = super::context()?;

    match command {
        PhpCommand::List => list(ui, &context),
        PhpCommand::ListVersions => list_versions(ui, &context),
        PhpCommand::Install { version } => install(ui, &mut context, version),
        PhpCommand::Use { version } => activate(ui, &context, &version),
        PhpCommand::Current => current(ui, &context),
        PhpCommand::Remove { version } => remove(ui, &context, &version),
        PhpCommand::Ini => ini(ui, &context),
        PhpCommand::Modules => modules(ui, &context),
        PhpCommand::Run { args } => passthrough(&context, args),
        PhpCommand::Passthrough(args) => passthrough(&context, args),
    }
}

fn list(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let rows = php::version_table(
        &context.paths,
        &context.catalog,
        context.platform,
        &context.config.sources,
    )?;
    if rows.is_empty() {
        ui.kv("installed", "none yet");
        ui.hint("`lambo php install 8.3` downloads and verifies one");
        return Ok(ExitCode::SUCCESS);
    }

    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let path = match &row.path {
                Some(path) => path.display().to_string(),
                // An uninstalled version has no path; say where it would come
                // from instead of printing a blank cell.
                None => match row.status {
                    php::VersionStatus::Unavailable => {
                        format!("only for {}", row.platform_text())
                    }
                    _ => "not installed".to_owned(),
                },
            };
            vec![
                row.version.clone(),
                row.status.as_str().to_owned(),
                // Empty for an installed version: where it came from is in its
                // manifest, and a column of "official" repeated down the screen
                // would push PATH off the terminal.
                row.source.clone().unwrap_or_default(),
                path,
            ]
        })
        .collect();
    ui.table(&["VERSION", "STATUS", "SOURCE", "PATH"], &table);

    let installed = rows.iter().filter(|row| row.path.is_some()).count();
    if installed == 0 {
        ui.hint("`lambo php install <version>` downloads, verifies and activates one");
    }
    Ok(ExitCode::SUCCESS)
}

fn list_versions(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let versions = php::available_versions(&context.catalog, context.platform);
    if versions.is_empty() {
        ui.warn(format!(
            "the catalogue has no PHP release for {}",
            context.platform.key()
        ));
        ui.hint(
            "add one with `lambo config`-managed catalogue overrides; see docs/configuration.md",
        );
        return Ok(ExitCode::SUCCESS);
    }
    for version in versions {
        ui.plain(version);
    }
    Ok(ExitCode::SUCCESS)
}

fn install(
    ui: &Ui,
    context: &mut lambo_core::session::Context<'_>,
    version: Option<String>,
) -> Result<ExitCode> {
    context.paths.ensure_layout()?;
    let spec = match version {
        Some(version) => version.parse::<VersionSpec>()?,
        None => VersionSpec::Stable,
    };

    ui.section(&format!("Installing PHP {spec}"));
    let runtime = php::install(
        &context.paths,
        &context.catalog,
        &spec,
        context.platform,
        context.downloader,
        &context.config.sources,
    )?;

    ui.ok(format!("installed PHP {}", runtime.name));
    ui.kv("path", runtime.path.display());
    ui.kv("php.ini", php::php_ini_path(&runtime, context.os).display());
    ui.kv("active", "yes");
    ui.hint("`lambo php -v` runs it without touching your PATH");
    Ok(ExitCode::SUCCESS)
}

fn activate(
    ui: &Ui,
    context: &lambo_core::session::Context<'_>,
    version: &str,
) -> Result<ExitCode> {
    let spec = version.parse::<VersionSpec>()?;
    let runtime = php::activate(&context.paths, &spec)?;
    ui.ok(format!("PHP {} is now active", runtime.name));
    Ok(ExitCode::SUCCESS)
}

fn current(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let Some(runtime) = php::current(&context.paths)? else {
        ui.warn("no PHP version is active");
        ui.hint("`lambo php install` installs one, `lambo php use <version>` picks one");
        return Ok(ExitCode::FAILURE);
    };
    let health = php::RuntimeHealth::check(&context.paths, &runtime, context.os);
    ui.kv("version", runtime.version.to_string());
    ui.kv("path", runtime.path.display());
    ui.kv(
        "executable",
        health
            .executable
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found".to_owned()),
    );
    // The point of this line is that it is not a restatement of the version
    // above: it is what PHP itself reported when Lambo started it. A version
    // that will not run shows up here rather than looking installed.
    ui.kv(
        "reported",
        health
            .reported_version
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "PHP did not run".to_owned()),
    );
    ui.kv(
        "configuration",
        health
            .loaded_ini
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none loaded".to_owned()),
    );
    if health.ran {
        ui.kv("extensions", health.modules.len());
    }
    if let Some(problem) = &health.problem {
        ui.warn(problem);
        ui.hint("`lambo doctor` explains what is wrong");
        return Ok(ExitCode::FAILURE);
    }
    if !health.ran {
        ui.fail(health.describe());
        ui.hint(format!(
            "`lambo php remove {}` then `lambo php install {}` reinstalls it",
            runtime.name, runtime.name
        ));
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn remove(ui: &Ui, context: &lambo_core::session::Context<'_>, version: &str) -> Result<ExitCode> {
    php::remove(&context.paths, version)?;
    ui.ok(format!("removed PHP {version}"));
    Ok(ExitCode::SUCCESS)
}

fn ini(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let runtime = require_active(context)?;
    ui.plain(php::php_ini_path(&runtime, context.os).display());
    Ok(ExitCode::SUCCESS)
}

fn modules(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<ExitCode> {
    let runtime = require_active(context)?;
    let health = php::RuntimeHealth::check(&context.paths, &runtime, context.os);
    // An empty module list used to be reported as "it may be broken", which
    // was a guess: the same empty list came back whether PHP would not start,
    // started and failed, or genuinely had no extensions. The health check
    // knows which of those happened and says so.
    if !health.ran {
        ui.fail(health.describe());
        ui.hint("`lambo doctor` explains what is wrong");
        return Ok(ExitCode::FAILURE);
    }
    for name in &health.modules {
        ui.plain(name);
    }
    Ok(ExitCode::SUCCESS)
}

/// Runs PHP with the user's arguments and exits with PHP's own status.
fn passthrough(context: &lambo_core::session::Context<'_>, args: Vec<String>) -> Result<ExitCode> {
    let runtime = require_active(context)?;
    let status = php::run(&context.paths, &runtime, &args, context.os)?;
    Ok(exit_code(status))
}

/// The active runtime, or an actionable failure.
fn require_active(context: &lambo_core::session::Context<'_>) -> Result<InstalledRuntime> {
    Ok(
        php::current(&context.paths)?.ok_or(lambo_core::Error::RuntimeMissing {
            kind: "PHP",
            command: "lambo php install",
        })?,
    )
}

/// A child's exit status as this process's exit code.
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
        // Signalled on Unix, or a status Windows did not report numerically.
        None => ExitCode::FAILURE,
    }
}
