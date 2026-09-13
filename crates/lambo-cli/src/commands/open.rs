//! `lambo open` - open the project (or the database manager) in a browser.
//!
//! Opening a URL that answers is the whole point, so the project URL is
//! checked first: opening a dead address teaches the user nothing except that
//! Lambo claimed more than it knew.

use std::process::ExitCode;

use lambo_core::session;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui, database: bool) -> Result<ExitCode> {
    let mut context = super::context()?;

    if database {
        let project = super::project_or_none()?;
        let name = project.as_ref().map(|project| project.database_name());
        let url = session::open_database_ui(&mut context, name.as_deref(), true)?;
        ui.ok(format!("opened {url}"));
        return Ok(ExitCode::SUCCESS);
    }

    let project = super::project()?;
    // From the port the server actually bound, not the configured one: after
    // a port fallback the two differ, and opening the configured URL would
    // land on an address nothing is listening on.
    let url = session::effective_url(&context.paths, &project, &context.config, context.os);
    if !lambo_core::http::is_up(&url) {
        ui.fail(format!("{url} is not answering"));
        ui.hint("start it with `lambo up`, or see what is wrong with `lambo status`");
        return Ok(ExitCode::FAILURE);
    }

    lambo_core::browser::open(&url, context.os)?;
    ui.ok(format!("opened {url}"));
    Ok(ExitCode::SUCCESS)
}
