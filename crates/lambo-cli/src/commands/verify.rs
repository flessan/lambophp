//! `lambo verify` - prove the installation, do not just describe it.
//!
//! The diagnosis suite says what is there; verification adds what a diagnosis
//! cannot see: whether the catalogue is releasable at all, and whether every
//! cached download still matches the digest the catalogue pins for it. A
//! machine that passes `verify` can install everything the catalogue promises
//! from what it already has, and a machine that fails it is told which bytes
//! to replace.

use std::process::ExitCode;

use lambo_core::doctor::{self, Report};

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui) -> Result<ExitCode> {
    let context = super::context()?;
    let project = super::project_or_none()?;
    let mut report = doctor::run(
        &context.paths,
        &context.config,
        project.as_ref(),
        &context.catalog,
        context.platform,
        context.os,
    );

    // The two gates that make this verification rather than description: a
    // catalogue whose entries cannot be installed is not an installation that
    // verifies, and a cache whose bytes drifted from their pins is not one
    // either.
    report
        .checks
        .push(doctor::check_release_catalog(&context.catalog));
    report.checks.push(doctor::check_cached_artifacts(
        &context.paths,
        &context.catalog,
    ));

    super::doctor::print_report(ui, &report);
    print_summary(ui, &report);

    Ok(ExitCode::from(report.exit_code().clamp(0, 255) as u8))
}

fn print_summary(ui: &Ui, report: &Report) {
    let (ok, unsupported, warn, fail) = report.counts();
    ui.section("Summary");
    ui.kv("passed", ok);
    if unsupported > 0 {
        ui.kv("unsupported", unsupported);
    }
    ui.kv("warnings", warn);
    ui.kv("failed", fail);
    if fail > 0 {
        ui.hint("verification failed: each failure above names the command that fixes it");
    } else {
        ui.hint("verification passed: this installation can serve what the catalogue promises");
    }
}
