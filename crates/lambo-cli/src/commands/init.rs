//! `lambo init` - adopt the current directory.
//!
//! The flagship "one command" moment: probe the directory for evidence of what
//! it is, translate that into a `lambo.yml`, initialize the installation
//! layout, and leave the user with a file they own and can edit.

use std::process::ExitCode;

use lambo_core::config::{Config, DatabaseKind};
use lambo_core::lambofile::Lambofile;
use lambo_core::project::{self, InitOptions, InitReport};
use lambo_core::version::VersionSpec;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(
    ui: &Ui,
    name: Option<String>,
    php: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    force: bool,
    dry_run: bool,
) -> Result<ExitCode> {
    let dir = super::current_dir()?;
    let context = super::context()?;
    // First-run experience: any write command initializes the installation.
    if !dry_run {
        context.paths.ensure_layout()?;
    }

    let options = InitOptions {
        name,
        php: php.as_deref().map(str::parse::<VersionSpec>).transpose()?,
        port,
        database: database
            .as_deref()
            .map(str::parse::<DatabaseKind>)
            .transpose()?,
        force,
        dry_run,
    };

    let report = project::init(&dir, &context.config, &options, context.os)?;
    print_outcome(ui, &report, dry_run);
    print_detection(ui, &report);

    // The written file is the source of truth for the summary, so it is read
    // back rather than re-deriving what `init` decided.
    let file = if dry_run {
        None
    } else {
        Lambofile::load(&dir)?
    };
    match file {
        Some(file) => {
            print_configuration(ui, &file, &context.config);
            print_next_steps(ui, file.server.port.unwrap_or(context.config.server.port));
        }
        None => {
            ui.section("Next steps");
            ui.bullet("re-run without --dry-run to write lambo.yml");
            ui.bullet("`lambo up` then starts everything");
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// What the command did to `lambo.yml`.
fn print_outcome(ui: &Ui, report: &InitReport, dry_run: bool) {
    if dry_run {
        ui.warn(format!(
            "would create `{}` (nothing was written)",
            report.path.display()
        ));
    } else if report.existing {
        ui.ok(format!("kept the existing `{}`", report.path.display()));
    } else {
        ui.ok(format!("created `{}`", report.path.display()));
    }
    if let Some(legacy) = &report.migrated_from {
        ui.bullet(format!(
            "converted from `{}` - the original file was left in place",
            legacy.display()
        ));
    }
}

/// Renders what was found, including the findings that ruled things out.
fn print_detection(ui: &Ui, report: &InitReport) {
    let detection = &report.detection;
    ui.section("Detected");
    ui.kv("framework", detection.framework.display_name());
    for probe in &detection.probes {
        let mark = if probe.found { "yes" } else { "no " };
        ui.bullet(format!("[{mark}] {}", probe.description));
    }
    if let Some(requirement) = &detection.php_requirement {
        ui.kv("php (composer)", requirement);
    }
    ui.kv(
        "database",
        if detection.needs_database {
            "needed"
        } else {
            "not needed"
        },
    );
}

/// Renders the configuration that was written.
fn print_configuration(ui: &Ui, file: &Lambofile, config: &Config) {
    ui.section("Configuration");
    if let Some(name) = &file.name {
        ui.kv("name", name);
    }
    ui.kv("php", file.php.to_string());
    ui.kv(
        "server",
        file.server.kind.unwrap_or(config.server.kind).as_str(),
    );
    ui.kv("document root", &file.server.document_root);
    match file.database.kind.unwrap_or(config.database.kind) {
        DatabaseKind::None => ui.kv("database", "disabled"),
        kind => {
            let name = file.database.name.clone().unwrap_or_default();
            ui.kv("database", format!("{} ({name})", kind.as_str()));
        }
    }
}

fn print_next_steps(ui: &Ui, port: u16) {
    ui.section("Next steps");
    ui.bullet("review lambo.yml - it is meant to be edited and committed");
    ui.bullet(format!(
        "`lambo up` starts the services and opens {}",
        // Through the shared helper, never formatted here. A hand-built
        // `http://localhost:{port}` prints `http://localhost:80`, which is a
        // URL no user would type and no browser needs - and it disagreed with
        // what `lambo status` printed for the same project.
        lambo_core::naming::local_url(port)
    ));
    ui.bullet("`lambo doctor` explains anything that is missing");
}
