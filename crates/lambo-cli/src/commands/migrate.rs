//! `lambo migrate` - move an existing king-PHP installation to Lambo.
//!
//! Nothing is deleted and no runtime is copied: the migration reports what it
//! converted and what has to be reinstalled, and leaves the old home alone so
//! the user can go back.

use std::process::ExitCode;

use lambo_core::migration;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui, dry_run: bool) -> Result<ExitCode> {
    let paths = super::paths()?;

    let report = if dry_run {
        migration::detect(&paths)
    } else {
        migration::run(&paths)?
    };

    if report.is_empty() {
        ui.ok("nothing to migrate - no king-PHP installation was found");
        return Ok(ExitCode::SUCCESS);
    }

    ui.plain(report.render());
    if dry_run {
        ui.hint("re-run without --dry-run to apply this");
    }

    Ok(ExitCode::SUCCESS)
}
