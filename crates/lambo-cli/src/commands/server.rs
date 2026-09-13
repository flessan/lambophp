//! `lambo server` - the web server on its own.
//!
//! `lambo up` is the normal path; these commands exist for the moments when
//! only the server is in the way, and they reuse exactly the code `up` runs,
//! so the two can never disagree about how a server is started or stopped.

use std::process::ExitCode;

use clap::Subcommand;
use lambo_core::session;
use lambo_core::state::names;

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Start the web server for this project.
    Start,

    /// Stop the web server.
    Stop,

    /// Restart the web server.
    Restart,

    /// Show whether the web server is running.
    Status,
}

pub fn run(ui: &Ui, command: ServerCommand) -> Result<ExitCode> {
    match command {
        ServerCommand::Start => start(ui),
        ServerCommand::Stop => stop(ui),
        ServerCommand::Restart => {
            stop(ui)?;
            start(ui)
        }
        ServerCommand::Status => status(ui),
    }
}

fn start(ui: &Ui) -> Result<ExitCode> {
    let project = super::project()?;
    let mut context = super::context()?;
    context.paths.ensure_layout()?;

    let started = session::start_server(&project, &mut context)?;
    ui.ok(&started.message);
    // The note comes before the URL: it explains why the URL is what it is.
    if let Some(note) = &started.note {
        ui.warn(note);
    }
    // From the port that was actually bound, not the configured one - after a
    // fallback those differ, and printing the configured URL would point the
    // user at an address nothing is listening on.
    ui.kv("url", started.url());
    Ok(ExitCode::SUCCESS)
}

fn stop(ui: &Ui) -> Result<ExitCode> {
    let mut context = super::context()?;
    ui.ok(session::stop_server(&mut context)?);
    Ok(ExitCode::SUCCESS)
}

fn status(ui: &Ui) -> Result<ExitCode> {
    let mut context = super::context()?;
    let status = session::status(None, &mut context)?;

    ui.section("Web server");
    let mut found = false;
    for service in status
        .services
        .iter()
        .filter(|service| service.name == names::APACHE || service.name == names::PHP_SERVER)
    {
        found = true;
        let line = format!("{}: {}", service.name, service.state_word());
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
    }
    if !found {
        ui.kv("state", "not started");
        ui.hint("`lambo server start` starts it, or `lambo up` starts everything");
    }

    Ok(ExitCode::SUCCESS)
}
