//! Minimal terminal UI helpers shared by all commands.
//!
//! Colors follow the `NO_COLOR` convention (https://no-color.org) and are
//! disabled when stdout is not a terminal, so piped output is always clean -
//! which matters on Windows as much as anywhere else, where `|` and `>` are
//! just as common. This module is deliberately tiny: a future GUI brings its
//! own rendering, and commands stay testable because they emit plain text
//! through this one seam.

use std::error::Error as _;
use std::fmt::Display;
use std::io::IsTerminal;

use crate::error::CliError;

// ANSI SGR codes, applied via [`Ui::paint`].
const GREEN: &str = "32";
const YELLOW: &str = "33";
const RED: &str = "31";
const CYAN_BOLD: &str = "1;36";
const BOLD: &str = "1";
const DIM: &str = "2";

/// Terminal styling context.
#[derive(Debug, Clone, Copy)]
pub struct Ui {
    color: bool,
}

impl Ui {
    /// Detects whether styling is appropriate for this process.
    pub fn detect() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
            && std::io::stdout().is_terminal();
        Self { color }
    }

    /// Wraps `text` in an ANSI style when colors are enabled.
    pub fn paint(&self, code: &str, text: impl Display) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Section header, e.g. `── Project ──────────────`.
    pub fn section(&self, title: &str) {
        println!("{}", self.paint(CYAN_BOLD, format!("── {title} ")));
    }

    /// Aligned key/value line inside a section.
    pub fn kv(&self, key: &str, value: impl Display) {
        let key = format!("{key:<14}");
        println!("  {} {}", self.paint(BOLD, key), value);
    }

    /// Unprefixed fact line inside a section.
    pub fn bullet(&self, text: impl Display) {
        println!("  {} {text}", self.paint(DIM, "·"));
    }

    /// Positive outcome.
    pub fn ok(&self, message: impl Display) {
        println!("{} {message}", self.paint(GREEN, "✔"));
    }

    /// Attention-worthy situation.
    pub fn warn(&self, message: impl Display) {
        println!("{} {message}", self.paint(YELLOW, "⚠"));
    }

    /// Something is broken.
    pub fn fail(&self, message: impl Display) {
        println!("{} {message}", self.paint(RED, "✖"));
    }

    /// Actionable suggestion, indented under the previous line.
    pub fn hint(&self, message: impl Display) {
        println!("  {} {message}", self.paint(DIM, "hint:"));
    }

    /// Machine-readable output: passthrough from a child process, a version
    /// number, a path. Printed exactly as given, with no decoration, so it can
    /// be piped into another program.
    pub fn plain(&self, text: impl Display) {
        println!("{text}");
    }

    /// An aligned column table.
    ///
    /// Columns are sized to their widest cell and separated by two spaces,
    /// which stays readable in a terminal and still survives being piped into
    /// `awk` or `grep`. Trailing whitespace is trimmed.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) {
        let mut widths: Vec<usize> = headers.iter().map(|header| header.len()).collect();
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                if index < widths.len() {
                    widths[index] = widths[index].max(cell.len());
                }
            }
        }

        let render = |cells: &[&str]| -> String {
            let line: String = cells
                .iter()
                .enumerate()
                .map(|(index, cell)| {
                    // The last column is not padded: no trailing whitespace.
                    if index + 1 == cells.len() {
                        (*cell).to_owned()
                    } else {
                        format!("{cell:<width$}  ", width = widths[index])
                    }
                })
                .collect();
            line.trim_end().to_owned()
        };

        println!("{}", self.paint(BOLD, render(headers)));
        for row in rows {
            let cells: Vec<&str> = row.iter().map(String::as_str).collect();
            println!("{}", render(&cells));
        }
    }
}

/// Renders a fatal error, its source chain, and every detail the core engine
/// attached - the possible causes and the command that fixes it.
pub fn report_error(err: &CliError) {
    let ui = Ui::detect();
    eprintln!("{} {err}", ui.paint(RED, "error:"));

    let mut source = err.source();
    while let Some(cause) = source {
        eprintln!("  {} {cause}", ui.paint(DIM, "caused by:"));
        source = cause.source();
    }

    // `Display` for a service failure names the service; the actionable part
    // lives in the details, and a user who only reads the first line learns
    // nothing they can act on.
    if let CliError::Core(core) = err {
        for detail in core.details() {
            eprintln!("  {detail}");
        }
    }
}
