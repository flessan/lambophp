//! `lambo up` - start the project.
//!
//! Every decision (what to start, in what order, how to prove it worked) is
//! made by [`lambo_core::session`]; this module only renders the steps.

use std::process::ExitCode;

use lambo_core::session;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui, open_browser: bool) -> Result<ExitCode> {
    let project = super::project()?;
    let mut context = super::context()?;
    context.paths.ensure_layout()?;

    ui.section(&format!("Starting {}", project.name()));
    let report = session::up(&project, &mut context, open_browser)?;
    super::print_steps(ui, &report);

    if let Some(url) = &report.url {
        ui.section("Ready");
        ui.kv("url", url);
        if !report.browser_opened {
            ui.hint("open it manually, or re-run without --no-browser");
        }
    }

    Ok(ExitCode::SUCCESS)
}
