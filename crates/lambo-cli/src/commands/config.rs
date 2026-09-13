//! `lambo config` - read and mutate the global configuration.
//!
//! All validation lives in `lambo_core::config`; this module only renders.
//! `set` never writes a partial file: validation errors abort before save.

use std::process::ExitCode;

use clap::Subcommand;
use lambo_core::config::Config;
use lambo_core::paths::Paths;

use crate::error::{CliError, Result};
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the effective configuration as YAML.
    Show,

    /// Print one configuration value.
    Get {
        /// Configuration key, e.g. `server.port`.
        key: String,
    },

    /// Set a configuration value (validated before saving).
    Set {
        /// Configuration key, e.g. `server.port`.
        key: String,
        /// New value, e.g. `8080`.
        value: String,
    },

    /// List every key with its current value.
    List,

    /// Print the path of the configuration file.
    Path,

    /// Open the configuration file in $EDITOR (validated on exit).
    Edit,

    /// Print the SHA-256 of a file, for pinning in a catalogue.
    ///
    /// Prints and stops. It deliberately does not write the value anywhere:
    /// a digest is a claim that these bytes are the right ones, and that claim
    /// needs a human to make it. A command that recorded the digest of whatever
    /// it was handed would turn every install into trust-on-first-use, which is
    /// the one thing checksums exist to prevent.
    Hash {
        /// File to digest, e.g. a downloaded runtime archive.
        file: String,
    },
}

pub fn run(ui: &Ui, command: ConfigCommand) -> Result<ExitCode> {
    let paths = super::paths()?;

    match command {
        ConfigCommand::Path => {
            ui.plain(paths.config_file().display());
        }
        ConfigCommand::Hash { file } => {
            let path = std::path::Path::new(&file);
            match lambo_core::sha256::sha256_file(path) {
                Ok(digest) => {
                    // The `sha256sum` layout, so the output can be dropped into
                    // a sidecar file or compared against one.
                    ui.plain(format!("{digest}  {}", path.display()));
                    ui.hint(
                        "record it in `catalogs/default.json` or a catalogue \
                         override after checking it against the publisher's own \
                         value",
                    );
                }
                Err(error) => {
                    ui.fail(error.to_string());
                    return Ok(ExitCode::FAILURE);
                }
            }
        }
        ConfigCommand::Show => {
            let config = Config::load(&paths)?;
            ui.plain(config.to_yaml()?);
        }
        ConfigCommand::List => {
            let config = Config::load(&paths)?;
            for key in Config::KEYS {
                ui.plain(format!("{key} = {}", config.get(key)?));
            }
        }
        ConfigCommand::Get { key } => {
            let config = Config::load(&paths)?;
            match config.get(&key) {
                Ok(value) => ui.plain(value),
                Err(e @ lambo_core::Error::UnknownConfigKey(_)) => {
                    ui.fail(e.to_string());
                    print_known_keys(ui);
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => return Err(e.into()),
            }
        }
        ConfigCommand::Set { key, value } => {
            paths.ensure_layout()?;
            let mut config = Config::load(&paths)?;
            match config.set(&key, &value) {
                Ok(()) => {
                    config.save(&paths)?;
                    ui.ok(format!("{key} = {}", config.get(&key)?));
                    if key == "database.password" {
                        ui.hint(
                            "the value is stored in Lambo's configuration, never in a project file",
                        );
                    }
                }
                Err(e @ lambo_core::Error::UnknownConfigKey(_)) => {
                    ui.fail(e.to_string());
                    print_known_keys(ui);
                    return Ok(ExitCode::FAILURE);
                }
                Err(e) => return Err(e.into()),
            }
        }
        ConfigCommand::Edit => edit(ui, &paths)?,
    }

    Ok(ExitCode::SUCCESS)
}

fn print_known_keys(ui: &Ui) {
    ui.hint(format!("known keys: {}", Config::KEYS.join(", ")));
}

fn edit(ui: &Ui, paths: &Paths) -> Result<()> {
    paths.ensure_layout()?;
    if !paths.config_file().is_file() {
        Config::default().save(paths)?;
        ui.ok(format!("created `{}`", paths.config_file().display()));
    }

    // The editor is the user's own program, started directly - no shell is
    // involved, so a path with spaces or a quoting surprise cannot turn into
    // something else being executed.
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| default_editor());

    let status = std::process::Command::new(&editor)
        .arg(paths.config_file())
        .status()
        .map_err(|source| CliError::Editor {
            program: editor.clone(),
            source,
        })?;

    if !status.success() {
        ui.warn("the editor exited with a non-zero status; changes were kept as-is");
    }

    // Never leave the user with a broken file silently: re-validate after
    // every editing session.
    match Config::load(paths) {
        Ok(_) => ui.ok("configuration is valid"),
        Err(e) => {
            ui.fail(format!("the configuration file has errors: {e}"));
            ui.hint("fix the file manually, or delete it to restore the defaults");
            return Err(e.into());
        }
    }
    Ok(())
}

/// The editor used when neither `VISUAL` nor `EDITOR` is set.
fn default_editor() -> String {
    if lambo_core::platform::Os::host().is_windows() {
        "notepad".to_owned()
    } else {
        "vi".to_owned()
    }
}
