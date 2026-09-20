//! `lambo frameworks` - the catalogue of scaffoldable frameworks.
//!
//! Three commands, all thin: listing renders the catalogue, creating scaffolds a
//! framework into the installation's `www/` directory, registers the project and
//! its virtual host and rewrites the hosts file, and deleting takes all of that
//! away again. Creating is one call into
//! [`lambo_core::session::create_project`] and deleting is one call into
//! [`lambo_core::session::delete_project`] - the same calls the GUI's projects
//! page makes, so the only difference between the two interfaces is how they
//! collect the framework, the name and the domain.

use std::process::ExitCode;
use std::sync::Arc;

use clap::Subcommand;
use lambo_core::frameworks;

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum FrameworksCommand {
    /// List every framework that can be scaffolded (the default).
    List,

    /// Scaffold a framework into the installation and serve it on a domain.
    Create {
        /// Framework name, exactly as `lambo frameworks` lists it,
        /// e.g. `Laravel`, `Next.js`, `Static HTML`.
        framework: String,

        /// Project name. Slugified into the directory under `www/`:
        /// `My Shop!` becomes `my-shop`.
        name: String,

        /// Domain to publish it on [default: `<name>.test`].
        #[arg(long)]
        domain: Option<String>,
    },

    /// Delete a project: its directory, its registration and its domain.
    Delete {
        /// Project name, as `lambo frameworks` and the projects page show it.
        name: String,
    },

    /// Move a project to another domain and publish it there.
    ///
    /// The project's files do not move: a domain is a name, and the document
    /// root stays the project's own directory.
    Domain {
        /// Project name, as `lambo frameworks` and the projects page show it.
        name: String,

        /// The domain it should answer on from now on.
        domain: String,
    },
}

pub fn run(ui: &Ui, command: Option<FrameworksCommand>) -> Result<ExitCode> {
    match command.unwrap_or(FrameworksCommand::List) {
        FrameworksCommand::List => list(ui)?,
        FrameworksCommand::Create {
            framework,
            name,
            domain,
        } => create(ui, &framework, &name, domain.as_deref())?,
        FrameworksCommand::Delete { name } => delete(ui, &name)?,
        FrameworksCommand::Domain { name, domain } => domain_change(ui, &name, &domain)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints the catalogue, and what of it can be scaffolded here.
fn list(ui: &Ui) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();

    ui.section("Frameworks");
    let rows: Vec<Vec<String>> = frameworks::frameworks()
        .iter()
        .map(|framework| {
            let tools = framework.required_tools.join(", ");
            let port = frameworks::proxy_port(framework.name);
            vec![
                framework.name.to_owned(),
                framework.runtime.to_owned(),
                if tools.is_empty() {
                    "-".to_owned()
                } else {
                    tools
                },
                if port == 0 {
                    "-".to_owned()
                } else {
                    port.to_string()
                },
                framework.description.to_owned(),
            ]
        })
        .collect();
    ui.table(
        &["Framework", "Runtime", "Needs", "Proxy", "What it is"],
        &rows,
    );

    // The runtimes the frameworks need, and whether each one is here. This is
    // the projects page's `Runtime status:` line.
    ui.kv("runtime status", frameworks::runtime_status_text(&base_dir));
    ui.kv("frameworks", frameworks::frameworks().len());
    ui.hint("lambo frameworks create <framework> <name> [--domain <domain>]");

    Ok(())
}

/// Scaffolds a framework and reports where it landed.
fn create(ui: &Ui, framework: &str, name: &str, domain: Option<&str>) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    // The installation itself may not exist yet on a fresh machine; the project
    // directory below `www/` is created by the scaffold.
    context.paths.ensure_layout()?;

    let log = Arc::clone(&context.log);
    let created =
        lambo_core::session::create_project(&base_dir, framework, name, domain.unwrap_or(""), log)?;

    ui.section("Created");
    ui.kv("framework", created.framework);
    ui.kv("project", created.name);
    ui.kv("directory", created.doc_root.display());
    ui.kv("domain", format!("http://{}", created.domain));
    if created.proxy_port > 0 {
        ui.kv(
            "proxied to",
            format!("the framework's own server on port {}", created.proxy_port),
        );
    }
    if let Some(warning) = &created.warning {
        ui.warn(warning);
    }
    ui.hint("restart the stack to pick up the new vhost: `lambo down`, then `lambo up`");

    Ok(())
}

/// Deletes a project and reports what went with it.
///
/// A deletion that has nothing to delete is not an error: the name is what the
/// user typed, and "there is no such project" is the answer to it.
fn delete(ui: &Ui, name: &str) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    let log = Arc::clone(&context.log);

    if lambo_core::session::delete_project(&base_dir, name, log)? {
        ui.ok(format!("deleted project `{name}`"));
        ui.hint("its domain is gone from the hosts file and the server configurations");
    } else {
        ui.bullet(format!("no project named `{name}` is registered"));
    }

    Ok(())
}

/// Moves a project to another domain, through the shared implementation.
///
/// The change is three things at once - the project row, its virtual host and
/// the files both are written from - so it is one core call, and it publishes
/// the result the way creating and deleting a project do.
fn domain_change(ui: &Ui, name: &str, domain: &str) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    let log = Arc::clone(&context.log);

    if lambo_core::session::set_project_domain(&base_dir, name, domain, log)? {
        ui.ok(format!("project `{name}` now answers on http://{domain}"));
        ui.hint("the project's directory did not move: only its name changed");
    } else {
        ui.bullet(format!("no project named `{name}` is registered"));
    }

    Ok(())
}
