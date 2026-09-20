//! Safe adapter from ProcessSpec to the Win32 handle-allowlisted launcher.
//!
//! Do not use Command::spawn for anything that outlives this process - a
//! detached service or a supervised one alike: selecting log-file or piped
//! stdio does not stop CreateProcess from inheriting unrelated CLI capture
//! handles, and a service holds whatever it inherits until it stops.

use std::fs::File;
use std::os::windows::io::AsHandle;
use std::process::Command;

use super::{Child, Output, ProcessSpec};
use crate::error::{Error, Result};

/// Starts a supervised service whose output the engine streams.
///
/// The launcher gives the child pipes it created and nothing else - the CLI's
/// own standard streams are not inheritable through this path, so a caller
/// capturing the CLI reaches EOF as soon as the CLI exits, with the service
/// still running.
pub(super) fn spawn_service(
    spec: &ProcessSpec,
) -> std::io::Result<lambo_process_windows::PipedChild> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.envs(&spec.env);
    lambo_process_windows::spawn_piped(&command, super::flags::CREATE_NO_WINDOW)
}

pub(super) fn spawn_detached(spec: &ProcessSpec) -> Result<std::io::Result<Child>> {
    let stdin = file_for(&spec.stdin, true)?;
    let stdout = file_for(&spec.stdout, false)?;
    let stderr = file_for(&spec.stderr, false)?;
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command.envs(&spec.env);
    Ok(lambo_process_windows::spawn(
        &command,
        [stdin.as_handle(), stdout.as_handle(), stderr.as_handle()],
    ))
}

fn file_for(output: &Output, input: bool) -> Result<File> {
    match output {
        Output::Inherit => Err(Error::InvalidInput(
            "detached services require file/null stdio, not inherited terminal handles".to_owned(),
        )),
        Output::Null => File::options()
            .read(input)
            .write(!input)
            .open(r"\\.\NUL")
            .map_err(|source| Error::io("NUL", source)),
        Output::File(path) => {
            if !input {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| Error::io(parent, source))?;
                }
            }
            File::options()
                .read(input)
                .create(!input)
                .append(!input)
                .open(path)
                .map_err(|source| Error::io(path, source))
        }
    }
}
