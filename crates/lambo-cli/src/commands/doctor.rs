//! `lambo doctor` - run the diagnostic suite and render the report.
//!
//! Every finding carries the command that fixes it; a diagnosis without an
//! action is just noise.

use std::process::ExitCode;

use lambo_core::doctor::{self, Report, Severity};

use crate::error::Result;
use crate::ui::Ui;

pub fn run(ui: &Ui, release: bool) -> Result<ExitCode> {
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

    // `--release` adds the gate rather than replacing the diagnosis: a release
    // that passes the gate on a machine whose home is broken is still a release
    // nobody can reproduce.
    if release {
        report
            .checks
            .push(doctor::check_release_catalog(&context.catalog));
    }

    print_report(ui, &report);
    print_summary(ui, &report);

    // `Report::exit_code` is an `i32` so the core stays free of `ExitCode`;
    // the conversion clamps rather than truncating silently.
    Ok(ExitCode::from(report.exit_code().clamp(0, 255) as u8))
}

/// Renders every check, grouped by nothing at all - the order matters more
/// than any heading, because it follows what `lambo up` does.
pub(crate) fn print_report(ui: &Ui, report: &Report) {
    ui.section("Diagnostics");
    for check in &report.checks {
        let line = format!("{}: {}", check.name, check.detail);
        match check.severity {
            Severity::Ok => ui.ok(line),
            // A platform limitation is information, not a problem, so it is
            // printed plainly rather than in a warning colour.
            Severity::Unsupported => ui.plain(format!(" [--] {line}")),
            Severity::Warn => ui.warn(line),
            Severity::Fail => ui.fail(line),
        }
        if let Some(fix) = &check.fix {
            ui.hint(fix);
        }
    }
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
        ui.hint("fix the failures above; each one names the command to run");
    } else if warn > 0 {
        ui.hint("warnings do not stop `lambo up`, but they are worth a look");
    }
}
