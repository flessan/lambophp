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
    if database {
        // One path, not two: `lambo open --db` and `lambo db open` are the
        // same request, so the second spelling is handed to the first rather
        // than reimplemented here. Anything else drifts - and had drifted:
        // this branch used to skip the layout check the other one makes.
        return crate::commands::db::run(ui, crate::commands::db::DbCommand::Open);
    }

    let context = super::context()?;
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
