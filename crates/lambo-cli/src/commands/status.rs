//! `lambo status` - also the bare `lambo` default. Answers the three
//! questions a developer has when they open a terminal: where am I, what does
//! Lambo know about this project, and what is actually running.
//!
//! "Running" is always the observed answer, never the recorded one: a service
//! whose process died between commands is reported as stopped.

use std::process::ExitCode;

use lambo_core::paths;
use lambo_core::runtime::{self, RuntimeKind};
use lambo_core::session::{self, ServiceStatus};

use crate::error::Result;
use crate::ui::Ui;

/// The runtimes `lambo status` reports, in the order a user thinks about them.
const REPORTED: [RuntimeKind; 4] = [
    RuntimeKind::Php,
    RuntimeKind::Apache,
    RuntimeKind::Mariadb,
    RuntimeKind::Mysql,
];

pub fn run(ui: &Ui) -> Result<ExitCode> {
    let mut context = super::context()?;
    let project = super::project_or_none()?;
    let status = session::status(project.as_ref(), &mut context)?;

    ui.section("Lambo PHP");
    ui.kv("version", env!("CARGO_PKG_VERSION"));
    ui.kv("home", context.paths.root().display());
    if std::env::var_os(paths::HOME_ENV).is_some() {
        ui.kv("home source", "LAMBO_HOME (portable or overridden)");
    }
    ui.kv("platform", context.platform.key());

    print_project(ui, project.as_ref(), &status, &context)?;
    print_services(ui, &status);
    print_runtimes(ui, &context)?;

    Ok(ExitCode::SUCCESS)
}

fn print_project(
    ui: &Ui,
    project: Option<&lambo_core::project::Project>,
    status: &session::Status,
    context: &lambo_core::session::Context<'_>,
) -> Result<()> {
    ui.section("Project");
    let Some(project) = project else {
        ui.kv("detected", "no lambo.yml here");
        ui.hint("run `lambo init` to adopt this directory");
        return Ok(());
    };
    ui.kv("name", project.name());
    ui.kv("root", project.root.display());
    let spec = project.php_spec(&context.config);
    match runtime::resolve(&context.paths, RuntimeKind::Php, &spec)? {
        Some(installed) => ui.kv("php", format!("{spec} → {} installed", installed.version)),
        // The requirement is still worth printing: it is what `lambo php
        // install` will go and get.
        None => {
            ui.kv("php", format!("{spec} (not installed)"));
            ui.hint(format!("`lambo php install {spec}` installs it"));
        }
    }
    ui.kv(
        "server",
        project.server_kind(&context.config).display_name(),
    );
    match project.database_kind(&context.config) {
        lambo_core::config::DatabaseKind::None => ui.kv("database", "disabled"),
        kind => ui.kv(
            "database",
            format!("{} ({})", kind.display_name(), project.database_name()),
        ),
    }
    ui.kv("document root", project.document_root().display());
    if let Some(url) = &status.url {
        ui.kv(
            "url",
            format!(
                "{url}{}",
                if status.serving {
                    "  (answering)"
                } else {
                    "  (not answering)"
                }
            ),
        );
    }
    Ok(())
}

fn print_services(ui: &Ui, status: &session::Status) {
    ui.section("Services");
    for service in &status.services {
        print_service(ui, service);
    }
}

fn print_service(ui: &Ui, service: &ServiceStatus) {
    let mut line = format!("{:<9} {}", service.name, service.state_word());
    if service.running {
        if let Some(pid) = service.pid {
            line.push_str(&format!(" (pid {pid})"));
        }
    }
    if let Some(port) = service.port {
        line.push_str(&format!(" on :{port}"));
    }
    if let Some(uptime) = &service.uptime {
        line.push_str(&format!(", up {uptime}"));
    }

    if service.running {
        ui.ok(line);
    } else {
        ui.bullet(line);
    }

    // A port held by something else is the most common reason `lambo up`
    // fails, so it is reported even when Lambo's own service is stopped.
    if let Some(occupant) = &service.occupant {
        if !service.running {
            ui.warn(format!(
                "port {} is currently held by {occupant}",
                service.port.unwrap_or(0)
            ));
        }
    }
    if let Some(log) = &service.log {
        ui.hint(format!("log: {}", log.display()));
    }
}

fn print_runtimes(ui: &Ui, context: &lambo_core::session::Context<'_>) -> Result<()> {
    ui.section("Runtimes");
    let mut any = false;
    for kind in REPORTED {
        let installed = runtime::installed(&context.paths, kind)?;
        if installed.is_empty() {
            continue;
        }
        any = true;
        let active = runtime::active_name(&context.paths, kind)?;
        let names: Vec<String> = installed
            .iter()
            .map(|runtime| {
                if active.as_deref() == Some(runtime.name.as_str()) {
                    format!("{} (active)", runtime.name)
                } else {
                    runtime.name.clone()
                }
            })
            .collect();
        ui.kv(kind.display_name(), names.join(", "));
        if let Some(active) = active {
            if installed.iter().all(|runtime| runtime.name != active) {
                ui.warn(format!(
                    "the active {} marker points at `{active}`, which is missing",
                    kind.display_name()
                ));
                ui.hint(format!("run `{}` to repair", kind.install_command()));
            }
        }
    }
    if !any {
        ui.kv("installed", "none yet");
        ui.hint("`lambo up` installs what the project needs, or use `lambo php install`");
    }
    Ok(())
}
