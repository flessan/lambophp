//! `lambo vhosts` - the virtual-hosts page, from the command line.
//!
//! Five commands, all thin: listing renders the rows the page shows, adding and
//! editing go through the page's own form validation, deleting removes a row,
//! and applying publishes the document. Every one of them is one call into
//! [`lambo_core::session`], which is where the rules live - the same calls the
//! virtual-hosts page makes, so the only difference between the two interfaces
//! is how they collect a domain, a document root, a port and a server.
//!
//! Nothing here is applied implicitly: saving a virtual host edits the document,
//! and `lambo vhosts apply` is what writes the hosts file and the server
//! configurations. That is the page's behaviour - its own button does it - and
//! it is what makes a batch of edits one publication instead of several.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Subcommand;
use lambo_core::frameworks::split_domain;
use lambo_core::panel::expand_path;
use lambo_core::vhost::{VhostForm, default_hosts_file, vhost_row};

use crate::error::Result;
use crate::ui::Ui;

#[derive(Debug, Subcommand)]
pub enum VhostsCommand {
    /// List the virtual hosts the installation publishes (the default).
    List,

    /// Add a virtual host and serve a directory on a domain.
    Add {
        /// Domain, e.g. `shop.test`. A name without a dot gets the default
        /// extension: `shop` becomes `shop.test`.
        domain: String,

        /// Document root, with `{base}` for the installation directory,
        /// e.g. `{base}/www/shop`.
        docroot: String,

        /// Port to serve it on [default: 80].
        #[arg(long, default_value_t = 80)]
        port: u16,

        /// Which web server answers: `apache`, `nginx` or `both`
        /// [default: apache].
        #[arg(long, default_value = "apache")]
        server: String,
    },

    /// Change a virtual host: its domain, document root, port or server.
    ///
    /// Everything not named stays as it is, exactly as editing the row in the
    /// page does.
    Edit {
        /// Domain of the row to change, as `lambo vhosts` lists it.
        domain: String,

        /// New domain.
        #[arg(long = "to")]
        to: Option<String>,

        /// New document root.
        #[arg(long)]
        docroot: Option<String>,

        /// New port.
        #[arg(long)]
        port: Option<u16>,

        /// New server: `apache`, `nginx` or `both`.
        #[arg(long)]
        server: Option<String>,
    },

    /// Remove a virtual host.
    ///
    /// The row goes from the document; run `lambo vhosts apply` to take it out
    /// of the hosts file and the server configurations as well.
    Delete {
        /// Domain, as `lambo vhosts` lists it.
        domain: String,
    },

    /// Write the hosts file and the server configurations ("Apply to System").
    Apply,
}

pub fn run(ui: &Ui, command: Option<VhostsCommand>) -> Result<ExitCode> {
    match command.unwrap_or(VhostsCommand::List) {
        VhostsCommand::List => list(ui)?,
        VhostsCommand::Add {
            domain,
            docroot,
            port,
            server,
        } => add(ui, &domain, &docroot, port, &server)?,
        VhostsCommand::Edit {
            domain,
            to,
            docroot,
            port,
            server,
        } => edit(
            ui,
            &domain,
            to.as_deref(),
            docroot.as_deref(),
            port,
            server.as_deref(),
        )?,
        VhostsCommand::Delete { domain } => delete(ui, &domain)?,
        VhostsCommand::Apply => apply(ui)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The form for a domain and a document root, as the page's fields would hold
/// it.
///
/// The domain is split at its last dot so that the extension field can be
/// filled the way selecting a row fills it, and a name without a dot takes the
/// default extension. Everything the form says is then validated by the page's
/// own rules, so a bad port or an empty document root is refused with the
/// message the page shows.
fn form_for(domain: &str, docroot: &str, port: u16, server: &str) -> VhostForm {
    let (name, extension) = split_domain(domain);
    VhostForm {
        name,
        extension,
        port: port.to_string(),
        server: server.to_owned(),
        docroot: docroot.to_owned(),
    }
}

/// A settings path as `vhost::apply` will actually use it.
///
/// An empty setting is not an empty file name: the hosts file falls back to the
/// platform default, and an empty server path means that file is not written at
/// all. Saying so is more useful than printing nothing, and it is what an apply
/// would do.
fn target(setting: &str, fallback: &str, base_dir: &Path) -> String {
    if setting.is_empty() {
        fallback.to_owned()
    } else {
        expand_path(setting, base_dir)
    }
}

/// The three files a change is published into, as the page's setting rows read
/// them.
fn print_targets(ui: &Ui, config: &lambo_core::panel::PanelConfig, base_dir: &Path) {
    let settings = &config.settings;
    ui.kv(
        "hosts file",
        target(
            &settings.hosts_file,
            &default_hosts_file().display().to_string(),
            base_dir,
        ),
    );
    ui.kv(
        "apache include",
        target(
            &settings.apache_vhosts_include,
            "(not configured - no file is written)",
            base_dir,
        ),
    );
    ui.kv(
        "nginx sites",
        target(
            &settings.nginx_sites_dir,
            "(not configured - no file is written)",
            base_dir,
        ),
    );
}

/// Prints the rows the virtual-hosts page shows, and where a change is written.
fn list(ui: &Ui) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    let hosts = lambo_core::session::vhosts(&base_dir)?;

    ui.section("Virtual hosts");
    let rows: Vec<Vec<String>> = hosts
        .iter()
        .map(|host| {
            let row = vhost_row(host, &base_dir);
            vec![
                row.marker.to_owned(),
                row.domain,
                row.docroot,
                row.port.to_string(),
                row.server,
            ]
        })
        .collect();
    ui.table(&["", "Domain", "Document Root", "Port", "Server"], &rows);

    let config = lambo_core::panel::PanelConfig::load(&base_dir)?;
    print_targets(ui, &config, &base_dir);
    if hosts.is_empty() {
        ui.bullet("no virtual hosts yet");
        ui.hint("lambo vhosts add shop.test {base}/www/shop");
    } else {
        ui.hint("lambo vhosts apply   # write them into the hosts file and the servers");
    }

    Ok(())
}

/// Adds a row, through the page's own form rules.
fn add(ui: &Ui, domain: &str, docroot: &str, port: u16, server: &str) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    context.paths.ensure_layout()?;

    let log = Arc::clone(&context.log);
    let stored = lambo_core::session::save_vhost(
        &base_dir,
        None,
        &form_for(domain, docroot, port, server),
        log,
    )?;

    ui.ok(format!("saved `{}`", stored.domain));
    ui.kv("document root", expand_path(&stored.docroot, &base_dir));
    ui.kv("port", stored.port);
    ui.kv(
        "server",
        if stored.server_type.is_empty() {
            "apache".to_owned()
        } else {
            stored.server_type.clone()
        },
    );
    ui.hint("publish it with `lambo vhosts apply`");

    Ok(())
}

/// Changes a row: the fields that were named, and nothing else.
fn edit(
    ui: &Ui,
    domain: &str,
    to: Option<&str>,
    docroot: Option<&str>,
    port: Option<u16>,
    server: Option<&str>,
) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();

    let Some(current) = lambo_core::session::vhosts(&base_dir)?
        .into_iter()
        .find(|host| host.domain == domain)
    else {
        ui.warn(format!("no virtual host for `{domain}`"));
        return Ok(());
    };

    // Filling the form from the row is what selecting it in the page does, so
    // an edit starts from the stored spelling (`{base}` and all) and only the
    // named fields change.
    let mut form = VhostForm::from_vhost(&current);
    if let Some(domain) = to {
        let (name, extension) = split_domain(domain);
        form.name = name;
        form.extension = extension;
    }
    if let Some(docroot) = docroot {
        form.docroot = docroot.to_owned();
    }
    if let Some(port) = port {
        form.port = port.to_string();
    }
    if let Some(server) = server {
        form.server = server.to_owned();
    }

    let log = Arc::clone(&context.log);
    let stored = lambo_core::session::save_vhost(&base_dir, Some(domain), &form, log)?;

    ui.ok(format!("`{domain}` is now `{}`", stored.domain));
    ui.kv("document root", expand_path(&stored.docroot, &base_dir));
    ui.kv("port", stored.port);
    ui.kv("enabled", stored.enabled);
    ui.hint("publish it with `lambo vhosts apply`");

    Ok(())
}

/// Removes a row from the document.
///
/// A domain that is not there is not an error: the name is what the user typed,
/// and "there is no such virtual host" is the answer to it.
fn delete(ui: &Ui, domain: &str) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    let log = Arc::clone(&context.log);

    if lambo_core::session::delete_vhost(&base_dir, domain, log)? {
        ui.ok(format!("removed `{domain}`"));
        ui.hint("publish the removal with `lambo vhosts apply`");
    } else {
        ui.bullet(format!("no virtual host for `{domain}` is registered"));
    }

    Ok(())
}

/// The page's "Apply to System".
fn apply(ui: &Ui) -> Result<()> {
    let context = super::context()?;
    let base_dir = context.install_dir();
    let log = Arc::clone(&context.log);

    lambo_core::session::apply_vhosts(&base_dir, log)?;

    let config = lambo_core::panel::PanelConfig::load(&base_dir)?;
    ui.ok("vhosts applied");
    print_targets(ui, &config, &base_dir);
    ui.hint("restart the stack to pick up server changes: `lambo down`, then `lambo up`");

    Ok(())
}
