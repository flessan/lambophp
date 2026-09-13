//! Command-line definition and dispatch.
//!
//! The shape of the CLI is declared once, here; handlers live in
//! [`crate::commands`] and delegate to `lambo-core`. Adding a command means:
//! declare it below, implement its module under `commands/`, wire the match
//! arm - nothing else. The checklist lives in CONTRIBUTING.md.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::commands;
use crate::commands::config::ConfigCommand;
use crate::commands::db::DbCommand;
use crate::commands::php::PhpCommand;
use crate::commands::server::ServerCommand;
use crate::commands::workspace::WorkspaceCommand;
use crate::error::Result;

/// A native local PHP development environment.
#[derive(Debug, Parser)]
#[command(
    name = "lambo",
    version,
    about = "A native local PHP development environment",
    long_about = "Lambo PHP - a native local PHP development environment.\n\n\
                  PHP, Apache and MariaDB managed for you: no Docker, no\n\
                  hand-edited httpd.conf or php.ini, no changes to your PATH.\n\
                  Everything lives under one directory and runs without\n\
                  administrator privileges.",
    after_help = "Getting started:\n  \
                  cd your-project\n  \
                  lambo init          # detect the project, write lambo.yml\n  \
                  lambo up            # start Apache + the database\n  \
                  open http://localhost\n\n\
                  Docs: https://github.com/flessan/kink-php-dev/tree/main/docs"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Detect this project and write lambo.yml.
    Init {
        /// Project name [default: composer package name or directory name].
        #[arg(long)]
        name: Option<String>,

        /// PHP version to use, e.g. `8.3`, `8.4`, `stable`.
        #[arg(long)]
        php: Option<String>,

        /// Port to serve on [default: the global `server.port`, 8080].
        #[arg(long)]
        port: Option<u16>,

        /// Database engine: `mariadb`, `mysql` or `none`.
        #[arg(long)]
        database: Option<String>,

        /// Overwrite an existing lambo.yml.
        #[arg(long)]
        force: bool,

        /// Show what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
    },

    /// Start the project's services (default: with a browser window).
    Up {
        /// Do not open the browser when the project is serving.
        #[arg(long)]
        no_browser: bool,
    },

    /// Stop every service Lambo started.
    Down,

    /// Restart the project's services.
    Restart {
        /// Do not open the browser when the project is serving.
        #[arg(long)]
        no_browser: bool,
    },

    /// Show project, service and runtime status (default command).
    Status,

    /// Open the project in a browser.
    Open {
        /// Open the database manager instead of the project.
        #[arg(long)]
        db: bool,
    },

    /// Diagnose the environment and suggest fixes.
    Doctor {
        /// Validate the catalogue as a release gate.
        ///
        /// Stricter than a normal diagnosis: an entry with no pinned SHA-256 is
        /// reported as an error, because a published catalogue whose entries
        /// cannot be installed is not shippable. Metadata only - nothing is
        /// downloaded. This is what the release pipeline runs.
        #[arg(long)]
        release: bool,
    },

    /// Show the logs of a Lambo service.
    Logs {
        /// Which log: `apache`, `database`, `php` or `lambo`.
        group: Option<String>,

        /// Keep printing as the log grows.
        #[arg(short, long)]
        follow: bool,

        /// How many trailing lines to show.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,

        /// Empty the log files instead of printing them.
        #[arg(long)]
        clear: bool,
    },

    /// Move an existing king-PHP installation to Lambo.
    Migrate {
        /// Show what would be migrated without changing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage PHP versions.
    #[command(subcommand)]
    Php(PhpCommand),

    /// Manage the web server on its own.
    #[command(subcommand)]
    Server(ServerCommand),

    /// Manage the database and the database manager.
    #[command(subcommand)]
    Db(DbCommand),

    /// Read and change Lambo configuration.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Manage workspaces: named groups of projects.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
}

/// Parses arguments, dispatches the command, returns the exit code.
pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse_from(normalized_argv());
    let ui = crate::ui::Ui::detect();

    match cli.command {
        None | Some(Command::Status) => commands::status::run(&ui),
        Some(Command::Init {
            name,
            php,
            port,
            database,
            force,
            dry_run,
        }) => commands::init::run(&ui, name, php, port, database, force, dry_run),
        Some(Command::Up { no_browser }) => commands::up::run(&ui, !no_browser),
        Some(Command::Down) => commands::down::run(&ui),
        Some(Command::Restart { no_browser }) => commands::restart::run(&ui, !no_browser),
        Some(Command::Open { db }) => commands::open::run(&ui, db),
        Some(Command::Doctor { release }) => commands::doctor::run(&ui, release),
        Some(Command::Logs {
            group,
            follow,
            lines,
            clear,
        }) => commands::logs::run(&ui, group.as_deref(), follow, lines, clear),
        Some(Command::Migrate { dry_run }) => commands::migrate::run(&ui, dry_run),
        Some(Command::Php(command)) => commands::php::run(&ui, command),
        Some(Command::Server(command)) => commands::server::run(&ui, command),
        Some(Command::Db(command)) => commands::db::run(&ui, command),
        Some(Command::Config(command)) => commands::config::run(&ui, command),
        Some(Command::Workspace(command)) => commands::workspace::run(&ui, command),
    }
}

/// Arguments that clap itself must answer, even after `lambo php`.
const OWN_FLAGS: [&str; 4] = ["-h", "--help", "-V", "--version"];

/// The process arguments, with `lambo php -v` rewritten to `lambo php run -v`.
///
/// clap reads a leading hyphen at the subcommand position as a flag of
/// `lambo php` and rejects it, but running the managed PHP directly is the
/// documented behaviour: `lambo php -v`, `lambo php -m`, `lambo php script.php`.
/// Argument handling is the CLI layer's job, so the rewrite happens here
/// instead of by loosening the parser for every command.
fn normalized_argv() -> Vec<OsString> {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    let Some(next) = args.get(2) else { return args };
    if args.get(1).map(OsString::as_os_str) != Some(OsString::from("php").as_os_str()) {
        return args;
    }
    let text = next.to_string_lossy();
    let is_php_flag = text.starts_with('-') && text != "-" && !OWN_FLAGS.contains(&text.as_ref());
    if is_php_flag {
        args.insert(2, OsString::from("run"));
    }
    args
}

/// The same rewrite, applied to an explicit argument list.
///
/// Kept separate from [`normalized_argv`] so the tests can drive it without
/// touching the real process arguments.
#[cfg(test)]
fn normalized(mut args: Vec<&str>) -> Vec<String> {
    let mut owned: Vec<OsString> = args.drain(..).map(OsString::from).collect();
    let next = owned.get(2).map(|arg| arg.to_string_lossy().into_owned());
    if owned.get(1).map(OsString::as_os_str) == Some(OsString::from("php").as_os_str()) {
        if let Some(next) = next {
            if next.starts_with('-') && next != "-" && !OWN_FLAGS.contains(&next.as_str()) {
                owned.insert(2, OsString::from("run"));
            }
        }
    }
    owned
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::normalized;
    use clap::Parser;

    #[test]
    fn php_flags_are_forwarded_to_php() {
        assert_eq!(
            normalized(vec!["lambo", "php", "-v"]),
            ["lambo", "php", "run", "-v"]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "-r", "echo 1;"]),
            ["lambo", "php", "run", "-r", "echo 1;"]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "script.php"]),
            ["lambo", "php", "script.php"],
            "a script name is already handled by the passthrough subcommand"
        );
    }

    #[test]
    fn the_documented_php_switches_reach_php() {
        // `-v`, `-m` and `--ini` are the switches the docs promise. Each one
        // starts with a hyphen at the subcommand position, which is exactly
        // what clap refuses to read as an argument.
        for invocation in [
            vec!["lambo", "php", "-v"],
            vec!["lambo", "php", "-m"],
            vec!["lambo", "php", "--ini"],
            vec!["lambo", "php", "--modules"],
            vec!["lambo", "php", "-i"],
        ] {
            let normalized = normalized(invocation.clone());
            assert_eq!(
                normalized.get(2).map(String::as_str),
                Some("run"),
                "{invocation:?} must be rewritten to `lambo php run …`, got {normalized:?}"
            );
            // And clap has to accept the rewrite, or the user sees a parse error.
            let cli = super::Cli::try_parse_from(normalized).unwrap();
            match cli.command {
                Some(super::Command::Php(super::PhpCommand::Run { args })) => {
                    assert_eq!(args, &invocation[2..], "{invocation:?} lost arguments");
                }
                other => panic!("{invocation:?} did not become a passthrough run: {other:?}"),
            }
        }
    }

    #[test]
    fn php_long_options_with_a_value_are_forwarded_intact() {
        assert_eq!(
            normalized(vec![
                "lambo",
                "php",
                "-d",
                "memory_limit=1G",
                "artisan",
                "migrate"
            ]),
            [
                "lambo",
                "php",
                "run",
                "-d",
                "memory_limit=1G",
                "artisan",
                "migrate"
            ]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "-c", "/etc/php/custom.ini", "-f"]),
            ["lambo", "php", "run", "-c", "/etc/php/custom.ini", "-f"]
        );
    }

    #[test]
    fn lambo_flags_are_left_for_lambo() {
        assert_eq!(
            normalized(vec!["lambo", "php", "--help"]),
            ["lambo", "php", "--help"]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "-h"]),
            ["lambo", "php", "-h"]
        );
        assert_eq!(normalized(vec!["lambo", "--help"]), ["lambo", "--help"]);
        // `lambo php -V` reports Lambo's version; PHP's is `lambo php -v`.
        // The two differ by case only, so the boundary is worth pinning down.
        assert_eq!(
            normalized(vec!["lambo", "php", "-V"]),
            ["lambo", "php", "-V"]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "--version"]),
            ["lambo", "php", "--version"]
        );
        assert_eq!(
            normalized(vec!["lambo", "php", "-v"]),
            ["lambo", "php", "run", "-v"]
        );
    }

    #[test]
    fn other_commands_are_untouched() {
        assert_eq!(
            normalized(vec!["lambo", "config", "set", "server.port", "8080"]),
            ["lambo", "config", "set", "server.port", "8080"]
        );
        assert_eq!(normalized(vec!["lambo"]), ["lambo"]);
        assert_eq!(normalized(vec!["lambo", "php"]), ["lambo", "php"]);
    }

    #[test]
    fn the_parsed_cli_accepts_a_rewritten_php_invocation() {
        // The rewrite is only useful if clap then accepts the result.
        let cli = super::Cli::try_parse_from(normalized(vec!["lambo", "php", "-v"])).unwrap();
        match cli.command {
            Some(super::Command::Php(super::PhpCommand::Run { args })) => {
                assert_eq!(args, ["-v"]);
            }
            other => panic!("expected a passthrough run, got {other:?}"),
        }
    }
}
