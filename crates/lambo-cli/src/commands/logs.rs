//! `lambo logs` - show what a service has been saying.
//!
//! Logs live under Lambo's own home, so this works identically on Windows and
//! on Unix and needs no knowledge of where Apache or MariaDB would otherwise
//! have written them.

use std::path::PathBuf;
use std::process::ExitCode;

use lambo_core::logs::{self, Group};
use lambo_core::paths::Paths;

use crate::error::Result;
use crate::ui::Ui;

/// The group shown when the user does not name one.
const DEFAULT_GROUP: Group = Group::Apache;

pub fn run(
    ui: &Ui,
    group: Option<&str>,
    follow: bool,
    lines: usize,
    clear: bool,
) -> Result<ExitCode> {
    let paths = super::paths()?;
    let group = match group {
        Some(name) => Group::parse(name).ok_or_else(|| {
            lambo_core::Error::InvalidInput(format!(
                "`{name}` is not a log group; expected one of: {}",
                Group::ALL
                    .iter()
                    .map(|group| group.dir_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?,
        None => DEFAULT_GROUP,
    };

    if clear {
        let cleared = logs::clear(&paths)?;
        ui.ok(format!(
            "emptied {cleared} log file(s) under {}",
            paths.logs_dir().display()
        ));
        return Ok(ExitCode::SUCCESS);
    }

    let file = log_file(&paths, group);
    if !file.is_file() {
        ui.warn(format!(
            "`{}` has not written anything yet",
            group.display_name()
        ));
        ui.hint(format!("expected at: {}", file.display()));
        return Ok(ExitCode::SUCCESS);
    }

    if follow {
        ui.warn(format!(
            "following {} - press Ctrl+C to stop",
            file.display()
        ));
        logs::follow(&file, &mut |line| println!("{line}"))?;
        return Ok(ExitCode::SUCCESS);
    }

    for line in logs::tail(&file, lines)? {
        ui.plain(line);
    }
    Ok(ExitCode::SUCCESS)
}

/// The file a group writes to.
fn log_file(paths: &Paths, group: Group) -> PathBuf {
    match group {
        Group::Apache => logs::apache(paths),
        Group::Database => logs::database(paths),
        Group::Php => logs::php(paths),
        Group::Lambo => logs::lambo(paths),
    }
}
