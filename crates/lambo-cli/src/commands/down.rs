//! `lambo down` - stop every service Lambo started.
//!
//! The installation's services, the current project's own server when it runs
//! one, and the leftovers a killed run left behind under the installation
//! directory. Nothing outside Lambo's own directories is touched.

use std::process::ExitCode;

use lambo_core::session;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui) -> Result<ExitCode> {
    let mut context = super::context()?;
    // Works with or without a project: a `server.kind: php` project's server is
    // stopped by name when we are inside one, and the sweep covers it either
    // way.
    let project = super::project_or_none()?;

    ui.section("Stopping");
    let report = session::down(project.as_ref(), &mut context)?;
    if report.steps.is_empty() {
        ui.bullet("nothing was running");
    } else {
        super::print_steps(ui, &report);
    }

    Ok(ExitCode::SUCCESS)
}
