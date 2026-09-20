//! Managed virtual hosts: the `hosts` file, Apache's include, nginx's sites.
//!
//! Every file this module writes is a *managed block* - a region between two
//! markers that the application owns and rewrites, surrounded by content the
//! user owns and that is never touched. That is the whole design:
//!
//! * **Nothing outside the markers is modified.** A hosts file with IPv6
//!   entries, comments and other tools' blocks survives every rewrite
//!   byte-for-byte, except that the file is re-emitted with CRLF line endings,
//!   which is what the previous implementation did and what Windows expects.
//! * **Re-applying is idempotent.** The existing block is removed before the
//!   new one is appended, so a domain removed from the configuration
//!   disappears from the hosts file rather than accumulating.
//! * **Files are written atomically** ([`crate::fsx`]). The hosts file is a
//!   system file: a crash during a non-atomic rewrite leaves a machine that
//!   cannot resolve its own name.
//!
//! The virtual-hosts page's own half lives here too - the form validation, the
//! row the list shows and the extension selection - because those are rules, not
//! drawing: `ui_tabs.go`'s `readVhostForm`, `refreshVhostList`'s cells and
//! `setDomainExtSelection` are ported beside the writers they feed.
//!
//! Ported from `vhost.go` and the vhosts page of `ui_tabs.go`.
//!
//! # Brand migration
//!
//! The markers say Lambo, and nginx files are written as `lambo-{domain}.conf`.
//! A file written by the previous implementation carries its markers and its
//! site files under its own prefix ([`LEGACY_NGINX_FILE_PREFIX`]); both are
//! removed on the next apply, so an
//! upgraded installation ends up with one block and one set of site files
//! rather than two competing ones. Two `VirtualHost` blocks on port 80 would
//! stop Apache from starting; two nginx `server` blocks on port 80 would do the
//! same to nginx.
//!
//! # A deliberate deviation
//!
//! The previous implementation read the hosts file with a line scanner whose
//! buffer was capped at 1 MiB: a single longer line silently truncated the file
//! from that point on, and the truncated result was then written back. Reading
//! here stops with an error instead, because losing the rest of a system hosts
//! file is not an acceptable failure mode. Nothing else about the rewrite
//! differs.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fsx;
use crate::panel::{PanelConfig, PanelProject, Vhost, expand_path};

/// The managed block's opening marker.
pub const HOSTS_MARKER_BEGIN: &str = "# >>> Lambo managed hosts BEGIN — do not edit this block <<<";

/// The managed block's closing marker.
pub const HOSTS_MARKER_END: &str = "# <<< Lambo managed hosts END >>>";

/// Apache's managed block opening marker.
pub const APACHE_MARKER_BEGIN: &str = "# >>> Lambo managed Apache vhosts BEGIN <<<";

/// Apache's managed block closing marker.
pub const APACHE_MARKER_END: &str = "# <<< Lambo managed Apache vhosts END >>>";

/// The previous implementation's hosts markers.
pub const LEGACY_HOSTS_MARKER_BEGIN: &str =
    "# >>> GoAMPP managed hosts BEGIN — do not edit this block <<<";

/// The previous implementation's hosts closing marker.
pub const LEGACY_HOSTS_MARKER_END: &str = "# <<< GoAMPP managed hosts END >>>";

/// The previous implementation's Apache markers.
pub const LEGACY_APACHE_MARKER_BEGIN: &str = "# >>> GoAMPP managed Apache vhosts BEGIN <<<";

/// The previous implementation's Apache closing marker.
pub const LEGACY_APACHE_MARKER_END: &str = "# <<< GoAMPP managed Apache vhosts END >>>";

/// The prefix of the nginx site files this module owns.
pub const NGINX_FILE_PREFIX: &str = "lambo-";

/// The prefix the previous implementation used.
pub const LEGACY_NGINX_FILE_PREFIX: &str = "goampp-";

/// The mark the virtual-hosts list shows for an enabled host.
const CHECK: &str = "\u{2713}";

/// The longest line the hosts file may contain.
///
/// The previous implementation's scanner had the same ceiling, but treated
/// exceeding it as "the file ends here"; this one refuses to rewrite a file it
/// cannot read in full.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// The hosts file Windows reads, from `SystemRoot`.
///
/// The environment variable is the platform's own answer to "where is Windows
/// installed"; `C:\Windows` is the fallback because a process without it
/// (a service, a stripped environment) would otherwise resolve nothing.
pub fn default_hosts_file() -> PathBuf {
    let system_root = std::env::var("SystemRoot")
        .ok()
        .filter(|root| !root.trim().is_empty())
        .unwrap_or_else(|| r"C:\Windows".to_owned());
    PathBuf::from(system_root)
        .join("System32")
        .join("drivers")
        .join("etc")
        .join("hosts")
}

/// Writes every managed file from a configuration.
///
/// The hosts file is always written; Apache's include and nginx's directory are
/// written only when the configuration names them, which is what makes an
/// Apache-only installation leave nginx alone and vice versa.
pub fn apply(base_dir: &Path, config: &PanelConfig) -> Result<()> {
    let enabled: Vec<&Vhost> = config.vhosts.iter().filter(|vhost| vhost.enabled).collect();

    let hosts = if config.settings.hosts_file.trim().is_empty() {
        default_hosts_file()
    } else {
        PathBuf::from(expand_path(&config.settings.hosts_file, base_dir))
    };
    write_hosts_block(&hosts, &enabled).map_err(|error| explain(error, "hosts file"))?;

    let apache = expand_path(&config.settings.apache_vhosts_include, base_dir);
    if !apache.is_empty() {
        write_apache_vhosts(Path::new(&apache), base_dir, &enabled)
            .map_err(|error| explain(error, "apache vhosts"))?;
    }

    let nginx = expand_path(&config.settings.nginx_sites_dir, base_dir);
    if !nginx.is_empty() {
        write_nginx_sites(Path::new(&nginx), base_dir, &enabled)
            .map_err(|error| explain(error, "nginx sites"))?;
    }

    Ok(())
}

/// Wraps an error with the file it concerns, the way the original prefixed its
/// messages.
fn explain(error: Error, what: &'static str) -> Error {
    Error::InvalidInput(format!("{what}: {error}"))
}

/// Rewrites the managed block of a hosts file.
///
/// The block is removed if present - either spelling of the markers - and the
/// enabled domains are appended. Two lines per domain, IPv4 and IPv6, because
/// a browser that resolves `localhost` to `::1` would otherwise miss the site.
pub fn write_hosts_block(path: &Path, vhosts: &[&Vhost]) -> Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Error::io(path, error)),
    };

    let mut cleaned = strip_managed_block(&existing, HOSTS_MARKER_BEGIN, HOSTS_MARKER_END)?;
    if !cleaned.is_empty() {
        cleaned =
            strip_managed_block(&cleaned, LEGACY_HOSTS_MARKER_BEGIN, LEGACY_HOSTS_MARKER_END)?;
    }

    let mut out = String::with_capacity(cleaned.len() + 128 * vhosts.len());
    out.push_str(&cleaned);
    if !cleaned.is_empty() && !cleaned.ends_with('\n') {
        out.push_str("\r\n");
    }
    out.push_str(HOSTS_MARKER_BEGIN);
    out.push_str("\r\n");
    for vhost in vhosts {
        out.push_str(&format!("127.0.0.1 {}\r\n", vhost.domain));
        out.push_str(&format!("::1       {}\r\n", vhost.domain));
    }
    out.push_str(HOSTS_MARKER_END);
    out.push_str("\r\n");

    fsx::write_atomic(path, &out)
}

/// Writes Apache's virtual-host include.
///
/// The first block is the bare `localhost` one, which is what makes
/// `http://localhost/` serve the website before any virtual host exists.
pub fn write_apache_vhosts(path: &Path, base_dir: &Path, vhosts: &[&Vhost]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fsx::ensure_dir(parent)?;
    }
    let default_docroot = slashes(&base_dir.join("www"));

    let mut out = String::with_capacity(1024 + 512 * vhosts.len());
    out.push_str(APACHE_MARKER_BEGIN);
    out.push_str("\r\n");
    out.push_str("# Generated by Lambo PHP — edits inside this file will be overwritten.\r\n\r\n");

    out.push_str("<VirtualHost *:80>\r\n");
    out.push_str("    ServerName localhost\r\n");
    out.push_str(&format!("    DocumentRoot \"{default_docroot}\"\r\n"));
    out.push_str(&format!("    <Directory \"{default_docroot}\">\r\n"));
    out.push_str("        Options Indexes FollowSymLinks\r\n");
    out.push_str("        AllowOverride All\r\n");
    out.push_str("        Require all granted\r\n");
    out.push_str("    </Directory>\r\n");
    out.push_str("</VirtualHost>\r\n\r\n");

    for vhost in vhosts {
        if !serves_apache(&vhost.server_type) || !vhost.enabled {
            continue;
        }
        let port = effective_port(vhost.port);

        out.push_str(&format!("<VirtualHost *:{port}>\r\n"));
        out.push_str(&format!("    ServerName {}\r\n", vhost.domain));

        if vhost.proxy_port > 0 {
            // A proxy host: the project runs its own server on `proxy_port`
            // and Apache forwards to it, which is what makes a Node.js or Go
            // project reachable at a `.test` domain.
            out.push_str("    ProxyPreserveHost On\r\n");
            out.push_str("    ProxyRequests Off\r\n");
            out.push_str(&format!(
                "    ProxyPass / http://127.0.0.1:{}/\r\n",
                vhost.proxy_port
            ));
            out.push_str(&format!(
                "    ProxyPassReverse / http://127.0.0.1:{}/\r\n",
                vhost.proxy_port
            ));
        } else {
            let docroot = slashes(Path::new(&expand_path(&vhost.docroot, base_dir)));
            out.push_str(&format!("    DocumentRoot \"{docroot}\"\r\n"));
            out.push_str(&format!("    <Directory \"{docroot}\">\r\n"));
            out.push_str("        Options Indexes FollowSymLinks\r\n");
            out.push_str("        AllowOverride All\r\n");
            out.push_str("        Require all granted\r\n");
            out.push_str("    </Directory>\r\n");
        }
        out.push_str("</VirtualHost>\r\n\r\n");
    }

    out.push_str(APACHE_MARKER_END);
    out.push_str("\r\n");
    fsx::write_atomic(path, &out)
}

/// Writes one nginx site file per enabled nginx virtual host, after removing
/// the ones this application wrote before.
///
/// The sweep is the reason files are prefixed: nginx's sites directory may hold
/// the user's own configuration, and only files this module wrote are ever
/// deleted. Both prefixes are swept, so an upgraded installation does not end
/// up with a stale legacy site file (see [`LEGACY_NGINX_FILE_PREFIX`])
/// listening on the same port.
pub fn write_nginx_sites(dir: &Path, base_dir: &Path, vhosts: &[&Vhost]) -> Result<()> {
    fsx::ensure_dir(dir)?;

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(NGINX_FILE_PREFIX) || name.starts_with(LEGACY_NGINX_FILE_PREFIX) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    for vhost in vhosts {
        if !serves_nginx(&vhost.server_type) {
            continue;
        }
        let docroot = slashes(Path::new(&expand_path(&vhost.docroot, base_dir)));
        let port = effective_port(vhost.port);
        let mut out = String::with_capacity(700);
        out.push_str("# Generated by Lambo PHP.\r\n");
        out.push_str("server {\r\n");
        out.push_str(&format!("    listen {port};\r\n"));
        out.push_str(&format!("    server_name {};\r\n", vhost.domain));
        out.push_str(&format!("    root {docroot};\r\n"));
        out.push_str("    index index.php index.html;\r\n\r\n");
        out.push_str(
            "    location / {\r\n        try_files $uri $uri/ /index.php?$query_string;\r\n    }\r\n\r\n",
        );
        out.push_str("    location ~ \\.php$ {\r\n");
        out.push_str("        fastcgi_pass 127.0.0.1:9000;\r\n");
        out.push_str("        fastcgi_index index.php;\r\n");
        out.push_str("        include fastcgi_params;\r\n");
        out.push_str(
            "        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;\r\n",
        );
        out.push_str("    }\r\n");
        out.push_str("}\r\n");

        let file = dir.join(format!(
            "{NGINX_FILE_PREFIX}{}.conf",
            safe_file_name(&vhost.domain)
        ));
        fsx::write_atomic(&file, &out)?;
    }
    Ok(())
}

/// Whether a virtual host's server type includes Apache.
///
/// An empty type means both, which is what a virtual host created before the
/// setting existed has.
pub fn serves_apache(server_type: &str) -> bool {
    matches!(server_type, "" | "apache" | "both")
}

/// Whether a virtual host's server type includes nginx.
pub fn serves_nginx(server_type: &str) -> bool {
    matches!(server_type, "" | "nginx" | "both")
}

/// The port a virtual host listens on, with the original's `0` meaning `80`.
pub fn effective_port(port: u16) -> u16 {
    if port == 0 { 80 } else { port }
}

/// A domain rendered safe to use as a file name.
///
/// Every character outside `[A-Za-z0-9._-]` becomes an underscore, so a
/// wildcard domain (`*.test`) or an internationalised one cannot escape the
/// sites directory or produce a name the filesystem rejects.
pub fn safe_file_name(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Removes a marker-delimited block from a file's text.
///
/// Line by line, and every surviving line is re-emitted with CRLF - which is
/// the previous implementation's behaviour and is what normalises a hosts file
/// written with Unix endings. The markers are recognised after trimming, so
/// indentation does not hide a block.
///
/// A closing marker without an opening one is also removed, as before: the
/// scanner treats both markers as "not a line" regardless of order.
pub fn strip_managed_block(text: &str, begin: &str, end: &str) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.len() > MAX_LINE_BYTES {
            return Err(Error::InvalidInput(format!(
                "a line of {MAX_LINE_BYTES} bytes or more cannot be rewritten safely"
            )));
        }
        // The last element of a text that ends with a line ending is not a
        // line at all: it is the artifact of the split, and the scanner the
        // original used never reported it.
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        // A scanner's line has its carriage return removed, and it is put back
        // when the line is written.
        let content = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = content.trim();

        if trimmed == begin {
            in_block = true;
            continue;
        }
        if trimmed == end {
            in_block = false;
            continue;
        }
        if in_block {
            continue;
        }

        out.push_str(content);
        out.push_str("\r\n");
    }
    Ok(out)
}

/// The virtual-host form's fields, as the page holds them between edits.
///
/// The port is text, not a number: the form has to be able to say "invalid
/// port: eight" about what the user actually typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VhostForm {
    /// The domain, without its extension.
    pub name: String,
    /// The selected extension, e.g. `.test`.
    pub extension: String,
    /// The port as typed.
    pub port: String,
    /// The selected server: `apache`, `nginx` or `both`.
    pub server: String,
    /// The document root, in the document's spelling (`{base}/...`).
    pub docroot: String,
}

impl VhostForm {
    /// The form as selecting a row fills it.
    ///
    /// The original split the domain at its last dot, selected the extension
    /// (falling back to the first entry), showed a zero port as 80, put the
    /// stored document root in the field unchanged, and mapped the server to the
    /// combo's index - `nginx` 1, `both` 2, anything else 0.
    pub fn from_vhost(vhost: &Vhost) -> Self {
        let (name, extension) = crate::frameworks::split_domain(&vhost.domain);
        let index = domain_extension_index(&extension);
        Self {
            name,
            extension: crate::frameworks::DOMAIN_EXTENSIONS[index].to_owned(),
            port: effective_port(vhost.port).to_string(),
            server: if vhost.server_type.is_empty() {
                "apache".to_owned()
            } else {
                vhost.server_type.clone()
            },
            docroot: vhost.docroot.clone(),
        }
    }

    /// The form as the page opens: an empty name, the default extension, port
    /// 80, Apache.
    pub fn blank() -> Self {
        Self {
            name: String::new(),
            extension: crate::frameworks::DOMAIN_EXTENSIONS[0].to_owned(),
            port: "80".to_owned(),
            server: "apache".to_owned(),
            docroot: String::new(),
        }
    }
}

/// Validates a form and turns it into a virtual host.
///
/// The original's `readVhostForm`, message for message. `enabled` is left
/// `false` because the caller decides: the page keeps the flag of the row it is
/// replacing, and enables anything new.
pub fn read_vhost_form(form: &VhostForm) -> Result<Vhost> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(Error::InvalidInput("domain name is required".to_owned()));
    }

    let extension = if form.extension.is_empty() {
        ".test"
    } else {
        form.extension.as_str()
    };

    let docroot = form.docroot.trim();
    if docroot.is_empty() {
        return Err(Error::InvalidInput("docroot is required".to_owned()));
    }

    let port_text = form.port.trim();
    let port_text = if port_text.is_empty() {
        "80"
    } else {
        port_text
    };
    // `Atoi` accepted a leading `+`, refused anything non-numeric, and the range
    // check refused zero and anything above 65535.
    let port: u16 = port_text
        .parse()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| Error::InvalidInput(format!("invalid port: {port_text}")))?;

    let server = if form.server.is_empty() {
        "apache"
    } else {
        form.server.as_str()
    };

    Ok(Vhost {
        domain: format!("{name}{extension}"),
        docroot: docroot.to_owned(),
        port,
        server_type: server.to_owned(),
        enabled: false,
        proxy_port: 0,
    })
}

/// One row of the virtual-hosts list, derived from a virtual host.
///
/// The list's five cells: the enabled mark, the domain, the *expanded* document
/// root, the port (a zero port is shown as 80) and the server (an empty type is
/// shown as `apache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VhostRow {
    /// The enabled mark: a check when the host is enabled, a space when not.
    pub marker: &'static str,
    /// The domain.
    pub domain: String,
    /// The document root, with `{base}` expanded to the installation.
    pub docroot: String,
    /// The port as shown: a zero port is 80.
    pub port: u16,
    /// The server as shown: an empty type is `apache`.
    pub server: String,
}

/// Builds the row the list shows for a virtual host.
pub fn vhost_row(vhost: &Vhost, base_dir: &Path) -> VhostRow {
    VhostRow {
        marker: if vhost.enabled { CHECK } else { " " },
        domain: vhost.domain.clone(),
        docroot: expand_path(&vhost.docroot, base_dir),
        port: effective_port(vhost.port),
        server: if vhost.server_type.is_empty() {
            "apache".to_owned()
        } else {
            vhost.server_type.clone()
        },
    }
}

/// Stores a saved virtual host in a document's list, in place.
///
/// `current` names the row the edit started from. That row is replaced and keeps
/// its enabled flag, so a host that was switched off is not switched back on by
/// editing it; a host that is not in the list is appended and enabled. This is
/// the page's Save handler, and it is the only place either rule is written.
pub fn store_vhost(vhosts: &mut Vec<Vhost>, current: Option<&str>, vhost: Vhost) -> Vhost {
    let existing = current.and_then(|domain| {
        vhosts
            .iter()
            .position(|candidate| candidate.domain == domain)
    });

    match existing {
        Some(index) => {
            let stored = Vhost {
                enabled: vhosts[index].enabled,
                ..vhost
            };
            vhosts[index] = stored.clone();
            stored
        }
        None => {
            let stored = Vhost {
                enabled: true,
                ..vhost
            };
            vhosts.push(stored.clone());
            stored
        }
    }
}

/// Removes a virtual host by domain, returning whether it was there.
///
/// Every host on the domain goes: the list is keyed by domain, and two rows for
/// one domain would be two publications of the same name.
pub fn remove_vhost(vhosts: &mut Vec<Vhost>, domain: &str) -> bool {
    let before = vhosts.len();
    vhosts.retain(|host| host.domain != domain);
    vhosts.len() != before
}

/// Moves a project, and every virtual host on its domain, to a new domain.
///
/// A project and its domains are two halves of one publication: the row the
/// projects page lists, and the entries the servers read. They move together, in
/// the one function that owns both, because moving either alone leaves a project
/// answering on a domain the list does not show.
///
/// Returns the updated project, or `None` when no project has that name. A
/// project with no host of its own still moves - the list is the user's, not the
/// document's.
pub fn move_project_domain(
    projects: &mut [PanelProject],
    vhosts: &mut [Vhost],
    name: &str,
    domain: &str,
) -> Option<PanelProject> {
    let project = projects.iter_mut().find(|project| project.name == name)?;
    let previous = std::mem::replace(&mut project.domain, domain.to_owned());
    let project = project.clone();

    for host in vhosts {
        if host.domain == previous {
            host.domain = domain.to_owned();
        }
    }
    Some(project)
}

/// A path with forward slashes, the form every generated configuration needs.
/// The index the extension combo selects for an extension.
///
/// Case-insensitive, and the first entry (`.test`) when the extension is not one
/// of the six, which is what the original's `setDomainExtSelection` did.
pub fn domain_extension_index(extension: &str) -> usize {
    crate::frameworks::DOMAIN_EXTENSIONS
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(extension))
        .unwrap_or(0)
}

/// A path with forward slashes, the form every generated configuration needs.
fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn project(name: &str, domain: &str) -> PanelProject {
        PanelProject {
            name: name.to_owned(),
            framework: "Laravel".to_owned(),
            domain: domain.to_owned(),
            docroot: format!("{{base}}/www/{name}"),
            port: 0,
        }
    }

    fn vhost(domain: &str) -> Vhost {
        Vhost {
            domain: domain.to_owned(),
            docroot: format!("{{base}}/www/{domain}"),
            port: 0,
            server_type: "apache".to_owned(),
            enabled: true,
            proxy_port: 0,
        }
    }

    #[test]
    fn the_shape_of_a_virtual_host_fixture_is_pinned() {
        // The fixture itself is behaviour: if the field names or defaults ever
        // change, every test below is describing a different product.
        let host = vhost("app.test");
        assert_eq!(host.domain, "app.test");
        assert!(host.enabled);
        assert_eq!(host.port, 0);
        assert_eq!(host.server_type, "apache");
        assert_eq!(host.proxy_port, 0);
        assert_eq!(effective_port(host.port), 80);
    }

    #[test]
    fn the_managed_hosts_block_is_created_with_both_addresses() {
        let temp = TempDir::new();
        let path = temp.join("hosts");

        let app = vhost("app.test");
        write_hosts_block(&path, &[&app]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            "# >>> Lambo managed hosts BEGIN — do not edit this block <<<\r\n\
             127.0.0.1 app.test\r\n\
             ::1       app.test\r\n\
             # <<< Lambo managed hosts END >>>\r\n"
        );
    }

    #[test]
    fn existing_content_is_preserved_and_the_block_is_appended() {
        let temp = TempDir::new();
        let path = temp.join("hosts");
        fs::write(
            &path,
            "# my own entries\n127.0.0.1 localhost\n# another tool's comment\n",
        )
        .unwrap();

        let app = vhost("app.test");
        write_hosts_block(&path, &[&app]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(
            "# my own entries\r\n127.0.0.1 localhost\r\n# another tool's comment\r\n"
        ));
        assert!(written.contains("# >>> Lambo managed hosts BEGIN"));
        assert!(written.contains("127.0.0.1 app.test"));
        // Unix endings become CRLF, which is what the original did.
        assert!(!written.contains("# my own entries\n"));
    }

    #[test]
    fn a_file_without_a_final_newline_gains_one() {
        let temp = TempDir::new();
        let path = temp.join("hosts");
        fs::write(&path, "# last line without a newline").unwrap();

        let app = vhost("app.test");
        write_hosts_block(&path, &[&app]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.starts_with("# last line without a newline\r\n# >>> Lambo managed hosts BEGIN")
        );
    }

    #[test]
    fn re_applying_replaces_the_block_instead_of_accumulating() {
        let temp = TempDir::new();
        let path = temp.join("hosts");
        fs::write(&path, "# keep me\n").unwrap();

        let first = vhost("one.test");
        let second = vhost("two.test");
        write_hosts_block(&path, &[&first]).unwrap();
        write_hosts_block(&path, &[&first, &second]).unwrap();
        write_hosts_block(&path, &[&second]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("BEGIN").count(), 1);
        assert_eq!(written.matches("END").count(), 1);
        assert!(!written.contains("one.test"), "a removed host must go");
        assert!(written.contains("127.0.0.1 two.test"));
        assert!(written.contains("# keep me\r\n"));
    }

    #[test]
    fn the_previous_implementations_block_is_replaced() {
        let temp = TempDir::new();
        let path = temp.join("hosts");
        fs::write(
            &path,
            "# mine\r\n\
             # >>> GoAMPP managed hosts BEGIN — do not edit this block <<<\r\n\
             127.0.0.1 old.test\r\n\
             ::1       old.test\r\n\
             # <<< GoAMPP managed hosts END >>>\r\n",
        )
        .unwrap();

        let app = vhost("app.test");
        write_hosts_block(&path, &[&app]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(!written.contains("GoAMPP"), "{written}");
        assert!(!written.contains("old.test"));
        assert_eq!(
            written.matches("127.0.0.1").count(),
            1,
            "the legacy entry is gone and the new one is there: {written}"
        );
        assert!(written.contains("127.0.0.1 app.test"));
    }

    #[test]
    fn an_empty_virtual_host_list_still_writes_the_block() {
        let temp = TempDir::new();
        let path = temp.join("hosts");
        write_hosts_block(&path, &[]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains(HOSTS_MARKER_BEGIN));
        assert!(written.contains(HOSTS_MARKER_END));
        assert!(!written.contains("127.0.0.1 "), "no domains: {written}");
    }

    #[test]
    fn a_missing_hosts_file_is_not_an_error_but_an_unreadable_one_is() {
        let temp = TempDir::new();
        // A directory where the file should be: `read` fails, and that must be
        // reported rather than silently replaced.
        let path = temp.join("hosts");
        fs::create_dir_all(&path).unwrap();
        assert!(write_hosts_block(&path, &[]).is_err());
    }

    #[test]
    fn a_line_too_long_to_read_safely_stops_the_rewrite() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let error = strip_managed_block(&long, HOSTS_MARKER_BEGIN, HOSTS_MARKER_END)
            .expect_err("a file that cannot be read in full must not be rewritten");
        assert!(
            error.to_string().contains("cannot be rewritten safely"),
            "{error}"
        );
    }

    #[test]
    fn the_apache_include_has_a_localhost_block_and_one_per_host() {
        let temp = TempDir::new();
        let path = temp.join("conf").join("apache").join("vhosts.conf");
        let app = vhost("app.test");
        write_apache_vhosts(&path, temp.path(), &[&app]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(APACHE_MARKER_BEGIN));
        assert!(written.ends_with(&format!("{APACHE_MARKER_END}\r\n")));
        assert!(
            written
                .contains("# Generated by Lambo PHP — edits inside this file will be overwritten.")
        );
        assert!(written.contains("<VirtualHost *:80>\r\n    ServerName localhost\r\n"));
        assert!(written.contains(&format!(
            "    DocumentRoot \"{}\"\r\n",
            slashes(&temp.path().join("www"))
        )));
        assert!(written.contains("<VirtualHost *:80>\r\n    ServerName app.test\r\n"));
        assert!(written.contains(&format!(
            "    DocumentRoot \"{}\"\r\n",
            slashes(&temp.path().join("www").join("app.test"))
        )));
        assert!(written.contains("        AllowOverride All\r\n"));
        assert_eq!(written.matches("</VirtualHost>").count(), 2);
        assert_eq!(written.matches("ProxyPass /").count(), 0);
    }

    #[test]
    fn a_proxy_host_forwards_instead_of_serving_files() {
        let temp = TempDir::new();
        let path = temp.join("vhosts.conf");
        let mut node = vhost("api.test");
        node.proxy_port = 3000;
        node.port = 8080;
        write_apache_vhosts(&path, temp.path(), &[&node]).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("<VirtualHost *:8080>\r\n    ServerName api.test\r\n"));
        assert!(written.contains("    ProxyPreserveHost On\r\n"));
        assert!(written.contains("    ProxyRequests Off\r\n"));
        assert!(written.contains("    ProxyPass / http://127.0.0.1:3000/\r\n"));
        assert!(written.contains("    ProxyPassReverse / http://127.0.0.1:3000/\r\n"));
        // The proxy host serves no files at all: the block the site is in has
        // no document root, while the default localhost block still has one.
        let block = written
            .split("<VirtualHost")
            .find(|block| block.contains("ServerName api.test"))
            .expect("the proxy host is written");
        assert!(!block.contains("DocumentRoot"), "{block}");
        assert!(written.contains("DocumentRoot"));
    }

    #[test]
    fn the_server_type_selects_which_files_a_host_appears_in() {
        let temp = TempDir::new();
        let apache_only = temp.join("apache.conf");

        let mut nginx_only = vhost("nginx.test");
        nginx_only.server_type = "nginx".to_owned();
        let mut both = vhost("both.test");
        both.server_type = "both".to_owned();
        let mut untyped = vhost("untyped.test");
        untyped.server_type = String::new();
        let mut disabled = vhost("off.test");
        disabled.enabled = false;

        let hosts = [&nginx_only, &both, &untyped, &disabled];
        write_apache_vhosts(&apache_only, temp.path(), &hosts).unwrap();
        let written = fs::read_to_string(&apache_only).unwrap();
        assert!(!written.contains("nginx.test"));
        assert!(written.contains("both.test"));
        assert!(written.contains("untyped.test"));
        assert!(!written.contains("off.test"));

        // `serves_*` is also exposed for the configuration editor.
        assert!(serves_apache("apache") && serves_apache("both") && serves_apache(""));
        assert!(!serves_apache("nginx"));
        assert!(serves_nginx("nginx") && serves_nginx("both") && serves_nginx(""));
        assert!(!serves_nginx("apache"));
    }

    #[test]
    fn nginx_site_files_are_named_after_the_domain() {
        let temp = TempDir::new();
        let dir = temp.join("sites");
        // The dashboard's default for a new host is Apache, so a host that is
        // to appear in nginx's sites directory says so.
        let mut app = vhost("app.test");
        app.server_type = "nginx".to_owned();
        let mut wildcard = vhost("*.wild.test");
        wildcard.server_type = "nginx".to_owned();
        wildcard.port = 8080;
        write_nginx_sites(&dir, temp.path(), &[&app, &wildcard]).unwrap();

        let app_file = fs::read_to_string(dir.join("lambo-app.test.conf")).unwrap();
        assert!(app_file.starts_with("# Generated by Lambo PHP.\r\nserver {\r\n"));
        assert!(app_file.contains("    listen 80;\r\n"));
        assert!(app_file.contains("    server_name app.test;\r\n"));
        assert!(app_file.contains(&format!(
            "    root {};\r\n",
            slashes(&temp.path().join("www").join("app.test"))
        )));
        assert!(app_file.contains("    index index.php index.html;\r\n"));
        assert!(app_file.contains("try_files $uri $uri/ /index.php?$query_string;"));
        assert!(app_file.contains("    location ~ \\.php$ {\r\n"));
        assert!(app_file.contains("        fastcgi_pass 127.0.0.1:9000;\r\n"));
        assert!(app_file.contains(
            "        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;\r\n"
        ));

        // A wildcard domain is escaped into a usable file name.
        let wildcard_file = dir.join("lambo-_.wild.test.conf");
        assert!(wildcard_file.is_file(), "{}", wildcard_file.display());
        assert!(
            fs::read_to_string(&wildcard_file)
                .unwrap()
                .contains("    listen 8080;\r\n")
        );
    }

    #[test]
    fn the_nginx_sweep_removes_both_prefixes_and_nothing_else() {
        let temp = TempDir::new();
        let dir = temp.join("sites");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lambo-old.test.conf"), "stale").unwrap();
        fs::write(dir.join("goampp-legacy.test.conf"), "stale").unwrap();
        fs::write(dir.join("my-own-site.conf"), "keep me").unwrap();
        fs::create_dir_all(dir.join("lambo-dir.conf")).unwrap();

        let mut app = vhost("app.test");
        app.server_type = "nginx".to_owned();
        write_nginx_sites(&dir, temp.path(), &[&app]).unwrap();

        assert!(!dir.join("lambo-old.test.conf").exists());
        assert!(
            !dir.join("goampp-legacy.test.conf").exists(),
            "the previous implementation's files are swept too"
        );
        assert!(
            dir.join("my-own-site.conf").is_file(),
            "the user's file stays"
        );
        assert!(
            dir.join("lambo-dir.conf").is_dir(),
            "directories are skipped"
        );
        assert!(dir.join("lambo-app.test.conf").is_file());
    }

    #[test]
    fn safe_file_names_keep_only_the_characters_that_are_always_safe() {
        assert_eq!(safe_file_name("app.test"), "app.test");
        assert_eq!(safe_file_name("my-app_2.test"), "my-app_2.test");
        assert_eq!(safe_file_name("*.wild.test"), "_.wild.test");
        assert_eq!(safe_file_name("nested/name"), "nested_name");
        assert_eq!(safe_file_name("a b"), "a_b");
        assert_eq!(safe_file_name("../../etc/passwd"), ".._.._etc_passwd");
        // One underscore per *character*: the original ranged over runes.
        assert_eq!(safe_file_name("ünïcode.test"), "_n_code.test");
        assert_eq!(safe_file_name(""), "");
    }

    #[test]
    fn stripping_a_block_leaves_the_markers_out_and_the_rest_in() {
        let text = "one\r\two\nbegin\ninside\nend\nthree";
        let stripped = strip_managed_block(text, "begin", "end").unwrap();
        assert_eq!(stripped, "one\r\two\r\nthree\r\n");
    }

    #[test]
    fn a_block_marker_is_recognised_after_trimming() {
        let text =
            format!("keep\n   {HOSTS_MARKER_BEGIN}  \n  inside\n\t{HOSTS_MARKER_END}\nkeep2\n");
        let stripped = strip_managed_block(&text, HOSTS_MARKER_BEGIN, HOSTS_MARKER_END).unwrap();
        assert_eq!(stripped, "keep\r\nkeep2\r\n");
    }

    #[test]
    fn stripping_an_empty_or_marker_free_file_changes_nothing_but_endings() {
        assert_eq!(strip_managed_block("", "a", "b").unwrap(), "");
        assert_eq!(
            strip_managed_block("x\ny\n", "a", "b").unwrap(),
            "x\r\ny\r\n"
        );
        assert_eq!(strip_managed_block("x\ny", "a", "b").unwrap(), "x\r\ny\r\n");
    }

    #[test]
    fn apply_writes_every_file_the_configuration_names() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        let mut site = vhost("site.test");
        // Both servers, so the run has to write both files.
        site.server_type = "both".to_owned();
        config.vhosts = vec![site];
        config.settings.hosts_file = temp.join("system-hosts").display().to_string();
        config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
        config.settings.nginx_sites_dir = "{base}/conf/nginx/sites".to_owned();

        apply(temp.path(), &config).unwrap();

        let hosts = fs::read_to_string(temp.join("system-hosts")).unwrap();
        assert!(hosts.contains("127.0.0.1 site.test"));
        assert!(temp.join("conf/apache/vhosts.conf").is_file());
        assert!(temp.join("conf/nginx/sites/lambo-site.test.conf").is_file());
    }

    #[test]
    fn apply_skips_the_web_server_files_a_configuration_does_not_name() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        config.settings.hosts_file = temp.join("hosts").display().to_string();
        config.settings.apache_vhosts_include = String::new();
        config.settings.nginx_sites_dir = String::new();

        apply(temp.path(), &config).unwrap();

        assert!(temp.join("hosts").is_file());
        assert!(!temp.join("conf/apache/vhosts.conf").exists());
        assert!(!temp.join("conf/nginx").exists());
    }

    #[test]
    fn apply_reports_which_file_failed() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        // A directory where the hosts file should be: the write fails, and the
        // message has to say which file and which step.
        let hosts = temp.join("hosts");
        fs::create_dir_all(&hosts).unwrap();
        config.settings.hosts_file = hosts.display().to_string();

        let error = apply(temp.path(), &config).unwrap_err();
        let text = error.to_string();
        assert!(text.starts_with("hosts file: "), "{text}");
        assert!(text.contains("hosts"), "{text}");
    }

    #[test]
    fn saving_a_host_keeps_the_flag_the_row_it_replaces_had() {
        let mut hosts = vec![vhost("app.test")];
        hosts[0].enabled = false;

        // Editing keeps it switched off: the page does not enable a host by
        // saving it.
        let mut edited = vhost("app.lan");
        edited.port = 8080;
        let stored = store_vhost(&mut hosts, Some("app.test"), edited);
        assert!(!stored.enabled);
        assert_eq!(hosts.len(), 1, "the row is replaced, not added");
        assert_eq!(hosts[0].domain, "app.lan");
        assert_eq!(hosts[0].port, 8080);

        // A host that was not in the list is added, and is on by default.
        let stored = store_vhost(&mut hosts, None, vhost("shop.test"));
        assert!(stored.enabled);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[1].domain, "shop.test");

        // Editing a row that has gone in the meantime adds it back, enabled:
        // there is nothing left to keep the flag of.
        let stored = store_vhost(&mut hosts, Some("gone.test"), vhost("gone.test"));
        assert!(stored.enabled);
        assert_eq!(hosts.len(), 3);
    }

    #[test]
    fn removing_a_host_takes_every_row_on_its_domain() {
        let mut hosts = vec![vhost("app.test"), vhost("shop.test"), vhost("app.test")];
        assert!(remove_vhost(&mut hosts, "app.test"));
        assert_eq!(
            hosts
                .iter()
                .map(|host| host.domain.as_str())
                .collect::<Vec<_>>(),
            vec!["shop.test"],
            "both rows on the domain go"
        );
        assert!(!remove_vhost(&mut hosts, "gone.test"), "nothing to remove");
        assert_eq!(hosts.len(), 1);
    }

    #[test]
    fn moving_a_project_moves_its_domain_and_every_host_on_it() {
        let mut projects = vec![project("shop", "shop.test")];
        let mut hosts = vec![vhost("shop.test"), vhost("other.test"), vhost("shop.test")];

        let moved = move_project_domain(&mut projects, &mut hosts, "shop", "shop.lan")
            .expect("the project is registered");
        assert_eq!(moved.name, "shop");
        assert_eq!(moved.domain, "shop.lan");
        assert_eq!(projects[0].domain, "shop.lan", "the row the page lists");
        assert_eq!(
            hosts
                .iter()
                .map(|host| host.domain.as_str())
                .collect::<Vec<_>>(),
            vec!["shop.lan", "other.test", "shop.lan"],
            "every host on the old domain moved, and nothing else did"
        );

        assert!(
            move_project_domain(&mut projects, &mut hosts, "gone", "gone.test").is_none(),
            "an unregistered project is reported, not created"
        );
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn a_project_domain_change_rewrites_every_managed_file() {
        let temp = TempDir::new();
        let base = temp.path();
        let mut config = PanelConfig::default_config();
        config.settings.hosts_file = "{base}/hosts".to_owned();
        config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
        config.settings.nginx_sites_dir = "{base}/conf/nginx/sites".to_owned();
        config.projects = vec![project("shop", "shop.test")];
        // The host the projects page registers: the project's directory as the
        // document root, which is a path and not a domain.
        let host = Vhost {
            domain: "shop.test".to_owned(),
            docroot: "{base}/www/shop".to_owned(),
            port: 80,
            server_type: "both".to_owned(),
            enabled: true,
            proxy_port: 0,
        };
        config.vhosts = vec![host];

        apply(base, &config).unwrap();

        let apache = fs::read_to_string(temp.join("conf/apache/vhosts.conf")).unwrap();
        assert!(apache.contains("ServerName shop.test"));

        move_project_domain(&mut config.projects, &mut config.vhosts, "shop", "shop.lan")
            .expect("the project is registered");
        apply(base, &config).unwrap();

        let hosts = fs::read_to_string(temp.join("hosts")).unwrap();
        assert!(hosts.contains("127.0.0.1 shop.lan"), "{hosts}");
        assert!(
            !hosts.contains("shop.test"),
            "the old domain is gone: {hosts}"
        );
        assert!(
            temp.join("conf/nginx/sites/lambo-shop.lan.conf").is_file(),
            "the site file followed the domain"
        );
        assert!(!temp.join("conf/nginx/sites/lambo-shop.test.conf").exists());
        let apache = fs::read_to_string(temp.join("conf/apache/vhosts.conf")).unwrap();
        assert!(apache.contains("ServerName shop.lan"));
        assert!(!apache.contains("shop.test"), "{apache}");
        // The files did not move: a domain is a name, and the document root is
        // the project's directory. Changing the domain republishes the project
        // under its new name, it does not relocate it. Generated configuration
        // carries forward slashes, whatever the platform spells paths with.
        let base_slashes = base.display().to_string().replace('\\', "/");
        assert!(
            apache.contains(&format!("DocumentRoot \"{base_slashes}/www/shop\"")),
            "the document root is untouched: {apache}"
        );
        let nginx = fs::read_to_string(temp.join("conf/nginx/sites/lambo-shop.lan.conf")).unwrap();
        assert!(nginx.contains(&format!("root {base_slashes}/www/shop;")));
    }

    #[test]
    fn a_list_row_is_what_the_page_shows() {
        let temp = TempDir::new();
        let base = temp.path();

        let mut host = vhost("app.test");
        host.docroot = "{base}/www/app".to_owned();
        host.port = 0;
        host.server_type = String::new();

        let row = vhost_row(&host, base);
        assert_eq!(row.marker, "\u{2713}");
        assert_eq!(row.domain, "app.test");
        assert_eq!(row.docroot, format!("{}/www/app", base.display()));
        assert_eq!(row.port, 80, "a zero port is shown as 80");
        assert_eq!(row.server, "apache", "an empty type is shown as apache");

        host.enabled = false;
        host.port = 8080;
        host.server_type = "nginx".to_owned();
        let row = vhost_row(&host, base);
        assert_eq!(row.marker, " ", "a disabled host has no check");
        assert_eq!(row.port, 8080);
        assert_eq!(row.server, "nginx");
    }

    #[test]
    fn the_form_is_filled_from_a_row_and_validated_back() {
        let mut host = vhost("shop.lan");
        host.docroot = "{base}/www/shop".to_owned();
        host.port = 0;
        host.server_type = "both".to_owned();

        let form = VhostForm::from_vhost(&host);
        assert_eq!(
            form.name, "shop",
            "the extension is split off at the last dot"
        );
        assert_eq!(form.extension, ".lan");
        assert_eq!(form.port, "80", "a zero port is shown as 80");
        assert_eq!(form.server, "both");
        assert_eq!(form.docroot, "{base}/www/shop", "the stored spelling");

        // Saving it back produces the same host, less the enabled flag the page
        // keeps for the row it is replacing.
        let saved = read_vhost_form(&form).expect("the form is valid");
        assert_eq!(saved.domain, host.domain);
        assert_eq!(saved.docroot, host.docroot);
        assert_eq!(saved.port, 80);
        assert_eq!(saved.server_type, "both");
        assert!(!saved.enabled);
        assert_eq!(saved.proxy_port, 0);
    }

    #[test]
    fn the_form_defaults_and_refusals_are_the_originals() {
        let defaults = VhostForm {
            name: "  app  ".to_owned(),
            extension: String::new(),
            port: String::new(),
            server: String::new(),
            docroot: "  {base}/www/app  ".to_owned(),
        };
        let host = read_vhost_form(&defaults).expect("the defaults are applied");
        assert_eq!(host.domain, "app.test");
        assert_eq!(host.port, 80);
        assert_eq!(host.server_type, "apache");
        assert_eq!(host.docroot, "{base}/www/app", "the field is trimmed");

        let base = VhostForm {
            name: "app".to_owned(),
            extension: ".test".to_owned(),
            port: "80".to_owned(),
            server: "apache".to_owned(),
            docroot: "{base}/www/app".to_owned(),
        };
        let refused = |form: VhostForm| {
            read_vhost_form(&form)
                .expect_err("the form is refused")
                .to_string()
        };

        assert_eq!(
            refused(VhostForm {
                name: "   ".to_owned(),
                ..base.clone()
            }),
            "domain name is required"
        );
        assert_eq!(
            refused(VhostForm {
                docroot: "  ".to_owned(),
                ..base.clone()
            }),
            "docroot is required"
        );
        for port in ["0", "eight", "70000", "-1", "80.5"] {
            assert_eq!(
                refused(VhostForm {
                    port: port.to_owned(),
                    ..base.clone()
                }),
                format!("invalid port: {port}")
            );
        }

        // A blank field is the default, not a refusal: the page pre-fills 80,
        // and an emptied field means the same.
        assert_eq!(
            read_vhost_form(&VhostForm {
                port: "  ".to_owned(),
                ..base.clone()
            })
            .expect("the default port")
            .port,
            80
        );
        // The bounds the original accepted.
        assert_eq!(
            read_vhost_form(&VhostForm {
                port: "+8080".to_owned(),
                ..base.clone()
            })
            .expect("a signed port parses")
            .port,
            8080
        );
        assert_eq!(
            read_vhost_form(&VhostForm {
                port: "65535".to_owned(),
                ..base
            })
            .expect("the highest port is allowed")
            .port,
            65535
        );
    }

    #[test]
    fn the_extension_selection_is_case_insensitive_and_falls_back() {
        assert_eq!(domain_extension_index(".test"), 0);
        assert_eq!(domain_extension_index(".LAN"), 3);
        assert_eq!(domain_extension_index(".site"), 5);
        assert_eq!(
            domain_extension_index(".com"),
            0,
            "not offered, so the first entry is selected"
        );
        assert_eq!(domain_extension_index(""), 0);

        let blank = VhostForm::blank();
        assert_eq!(blank.extension, ".test");
        assert_eq!(blank.port, "80");
        assert_eq!(blank.server, "apache");
        assert!(blank.name.is_empty() && blank.docroot.is_empty());
    }

    #[test]
    fn dropping_a_virtual_host_removes_it_from_every_managed_file() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        config.settings.hosts_file = "{base}/hosts".to_owned();
        config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
        config.settings.nginx_sites_dir = "{base}/conf/nginx/sites".to_owned();
        // Both servers, so every managed file exists before it is taken away.
        let mut site = vhost("app.test");
        site.server_type = "both".to_owned();
        config.vhosts = vec![site];

        apply(temp.path(), &config).unwrap();
        let hosts = temp.join("hosts");
        let sites = temp.join("conf/nginx/sites");
        assert!(
            fs::read_to_string(&hosts).unwrap().contains("app.test"),
            "the domain is published"
        );
        assert!(sites.join("lambo-app.test.conf").is_file());
        assert!(temp.join("conf/apache/vhosts.conf").is_file());

        // Disabling it takes it out of the hosts file and sweeps its site file
        // away, which is what "disabled" has to mean for a server that reads
        // the directory.
        config.vhosts[0].enabled = false;
        apply(temp.path(), &config).unwrap();
        let written = fs::read_to_string(&hosts).unwrap();
        assert!(!written.contains("app.test"), "{written}");
        assert!(
            written.contains(HOSTS_MARKER_BEGIN),
            "the block itself stays"
        );
        assert!(!sites.join("lambo-app.test.conf").exists());

        // And removing it leaves an empty managed block rather than a stale one.
        config.vhosts.clear();
        apply(temp.path(), &config).unwrap();
        let written = fs::read_to_string(&hosts).unwrap();
        assert_eq!(
            written,
            format!("{HOSTS_MARKER_BEGIN}\r\n{HOSTS_MARKER_END}\r\n"),
            "nothing but the markers is left"
        );
    }

    #[test]
    fn applying_twice_leaves_no_temporary_files_behind() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        config.settings.hosts_file = "{base}/hosts".to_owned();
        config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
        config.settings.nginx_sites_dir = "{base}/conf/nginx/sites".to_owned();
        let mut site = vhost("app.test");
        site.server_type = "both".to_owned();
        site.proxy_port = 3000;
        config.vhosts = vec![site];

        apply(temp.path(), &config).unwrap();
        apply(temp.path(), &config).unwrap();

        let mut leftovers = Vec::new();
        for directory in [
            temp.path().to_path_buf(),
            temp.join("conf/apache"),
            temp.join("conf/nginx/sites"),
        ] {
            for entry in fs::read_dir(&directory)
                .expect("the directory exists")
                .flatten()
            {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".tmp") {
                    leftovers.push(format!("{}/{name}", directory.display()));
                }
            }
        }
        assert!(
            leftovers.is_empty(),
            "temporary files left behind: {leftovers:?}"
        );

        // The second apply replaced the files rather than appending to them.
        let hosts = fs::read_to_string(temp.join("hosts")).unwrap();
        assert_eq!(hosts.matches(HOSTS_MARKER_BEGIN).count(), 1);
        assert_eq!(hosts.matches("app.test").count(), 2, "IPv4 and IPv6");
        let apache = fs::read_to_string(temp.join("conf/apache/vhosts.conf")).unwrap();
        assert_eq!(apache.matches(APACHE_MARKER_BEGIN).count(), 1);
        assert_eq!(apache.matches("ServerName app.test").count(), 1);
    }

    #[test]
    fn the_settings_paths_are_expanded_from_the_installation() {
        let temp = TempDir::new();
        let mut config = PanelConfig::default_config();
        config.settings.hosts_file = "{base}/system/hosts".to_owned();
        config.settings.apache_vhosts_include = "{base}/conf/apache/vhosts.conf".to_owned();
        config.settings.nginx_sites_dir = "{base}/conf/nginx/sites".to_owned();
        let mut site = vhost("app.test");
        site.server_type = "both".to_owned();
        config.vhosts = vec![site];

        apply(temp.path(), &config).unwrap();

        assert!(temp.join("system/hosts").is_file(), "the hosts path");
        assert!(temp.join("conf/apache/vhosts.conf").is_file());
        assert!(temp.join("conf/nginx/sites/lambo-app.test.conf").is_file());

        // An absolute path is used as it is.
        let absolute = temp.join("elsewhere/hosts");
        config.settings.hosts_file = absolute.display().to_string();
        apply(temp.path(), &config).unwrap();
        assert!(absolute.is_file(), "an absolute hosts path is honoured");
    }

    #[test]
    fn the_default_hosts_file_follows_the_windows_directory() {
        let path = default_hosts_file();
        let text = path.to_string_lossy();
        assert!(
            text.ends_with("System32/drivers/etc/hosts")
                || text.ends_with(r"System32\drivers\etc\hosts"),
            "{text}"
        );
        // `SystemRoot` is used when the platform provides it.
        if let Ok(root) = std::env::var("SystemRoot") {
            if !root.trim().is_empty() {
                assert!(text.starts_with(&root), "{text} must start with {root}");
            }
        }
    }
}
