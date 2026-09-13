//! `lambo down` - stop every service Lambo started.
//!
//! Nothing else on the machine is touched: only processes with a record in
//! Lambo's own state file are stopped.

use std::process::ExitCode;

use lambo_core::session;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui) -> Result<ExitCode> {
    let mut context = super::context()?;

    ui.section("Stopping");
    let report = session::down(&mut context)?;
    if report.steps.is_empty() {
        ui.bullet("nothing was running");
    } else {
        super::print_steps(ui, &report);
    }

    Ok(ExitCode::SUCCESS)
}
