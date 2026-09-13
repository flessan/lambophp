//! `lambo restart` - stop and start the project.
//!
//! Implemented as `down` followed by `up` rather than as a signal to the
//! services, because the point of a restart in development is a clean slate:
//! regenerated configuration, a re-checked port, a re-validated `httpd.conf`.

use std::process::ExitCode;

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui, open_browser: bool) -> Result<ExitCode> {
    super::down::run(ui)?;
    super::up::run(ui, open_browser)
}
