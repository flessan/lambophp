//! `lambo workspace` - named groups of projects.
//!
//! A workspace is a list of directories and nothing more: it holds no
//! configuration of its own, so a project behaves identically inside and
//! outside one.

use std::process::ExitCode;

use clap::Subcommand;
use lambo_core::workspace::{DEFAULT_WORKSPACE, Workspaces};

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces and the projects in them.
    List,

    /// Add a project to a workspace.
    Add {
        /// Project directory [default: the current directory].
        path: Option<String>,

        /// Workspace to add it to [default: `default`].
        #[arg(short, long)]
        workspace: Option<String>,
    },

    /// Remove a project from a workspace.
    Remove {
        /// Project directory [default: the current directory].
        path: Option<String>,

        /// Workspace to remove it from [default: `default`].
        #[arg(short, long)]
        workspace: Option<String>,
    },
}

pub fn run(ui: &Ui, command: WorkspaceCommand) -> Result<ExitCode> {
    let paths = super::paths()?;

    match command {
        WorkspaceCommand::List => {
            let workspaces = Workspaces::load(&paths)?;
            if workspaces.is_empty() {
                ui.kv("workspaces", "none yet");
                ui.hint("`lambo workspace add` adds the current project");
                return Ok(ExitCode::SUCCESS);
            }
            for (name, projects) in workspaces.iter() {
                ui.section(name);
                for project in projects {
                    ui.bullet(project.display());
                }
            }
            ui.kv("projects", workspaces.project_count());
        }
        WorkspaceCommand::Add { path, workspace } => {
            paths.ensure_layout()?;
            let (name, project) = resolve(path, workspace)?;
            let mut workspaces = Workspaces::load(&paths)?;
            if workspaces.add(&name, &project) {
                workspaces.save(&paths)?;
                ui.ok(format!("added {} to workspace `{name}`", project.display()));
            } else {
                ui.bullet(format!(
                    "{} is already in workspace `{name}`",
                    project.display()
                ));
            }
        }
        WorkspaceCommand::Remove { path, workspace } => {
            let (name, project) = resolve(path, workspace)?;
            let mut workspaces = Workspaces::load(&paths)?;
            if workspaces.remove(&name, &project)? {
                workspaces.save(&paths)?;
                ui.ok(format!(
                    "removed {} from workspace `{name}`",
                    project.display()
                ));
            } else {
                ui.bullet(format!(
                    "{} was not in workspace `{name}`",
                    project.display()
                ));
            }
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Turns the optional arguments into an absolute project path and a name.
fn resolve(
    path: Option<String>,
    workspace: Option<String>,
) -> Result<(String, std::path::PathBuf)> {
    let dir = match path {
        Some(path) => std::path::PathBuf::from(path),
        None => super::current_dir()?,
    };
    // Canonicalizing makes the registry stable: `lambo workspace add .` from
    // two different shells records the same entry, and `remove` finds it.
    let project = std::fs::canonicalize(&dir).unwrap_or(dir);
    Ok((
        workspace.unwrap_or_else(|| DEFAULT_WORKSPACE.to_owned()),
        project,
    ))
}
