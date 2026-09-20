//! The Apache httpd service.
//!
//! Apache is the web server in the XAMPP-alternative story, so Lambo owns the
//! whole lifecycle: where it is installed, what configuration it runs with,
//! how it starts, how it stops, and how we know it is actually serving.
//!
//! # The configuration is generated, never edited
//!
//! `lambo server start` writes `$LAMBO_HOME/config/apache/httpd.conf` from
//! `lambo.yml` on every run. Users do not edit Apache configuration for normal
//! development, which means:
//!
//! - no drift between what the file says and what Lambo believes,
//! - a broken file cannot survive a restart (it is regenerated),
//! - the same code path works on Windows and Unix.
//!
//! # Windows details that are easy to get wrong
//!
//! - Apache's configuration parser treats `\` as an escape character, so every
//!   path is written with forward slashes ([`crate::paths::to_forward_slashes`])
//!   and quoted when it contains spaces.
//! - `ServerRoot` must be the Apache installation directory; `PidFile`,
//!   `ScoreBoardFile` and the logs are redirected into the Lambo home so a
//!   system-wide Apache and a Lambo Apache can coexist.
//! - PHP is wired in through `mod_php` (`php8apache2_4.dll`) and `PHPIniDir`,
//!   pointing at the PHP runtime the project selected.
//! - The process is started detached with `CREATE_NEW_PROCESS_GROUP`, and
//!   stopped with `httpd -k stop` before any `taskkill`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Family, Release};
use crate::config::ServerKind;
use crate::download::Downloader;
use crate::error::{Error, Result};
use crate::paths::{self, Paths};
use crate::platform::Os;
use crate::process::{self, Output, ProcessSpec};
use crate::runtime::{self, InstalledRuntime, RuntimeKind};
use crate::version::VersionSpec;

/// A discovered Apache installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Apache {
    /// The managed runtime, when Apache was installed by Lambo.
    pub runtime: Option<InstalledRuntime>,
    /// The `httpd` executable.
    pub executable: PathBuf,
    /// Apache's `ServerRoot`.
    pub server_root: PathBuf,
}

impl Apache {
    /// Human-readable description, e.g. `Apache 2.4.62 (managed)`.
    pub fn describe(&self) -> String {
        match &self.runtime {
            Some(runtime) => format!("Apache {} (managed by Lambo)", runtime.version),
            None => "Apache (system installation)".to_owned(),
        }
    }
}

/// Everything needed to generate an Apache configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Apache to configure.
    pub apache: Apache,
    /// Port to listen on.
    pub port: u16,
    /// Document root (absolute).
    pub document_root: PathBuf,
    /// Project name, used for the server name and log prefix.
    pub project_name: String,
    /// PHP integration, when the project runs PHP through Apache.
    pub php: Option<PhpIntegration>,
    /// Whether to allow `.htaccess` overrides (Laravel, WordPress, …).
    pub allow_override: bool,
    /// Directory index files, in order.
    pub directory_index: Vec<String>,
    /// URL paths served from outside the document root.
    ///
    /// This is how `http://localhost/phpmyadmin` works: the database manager is
    /// an application Lambo manages, not a file in the user's project, so it is
    /// mounted into the same site rather than given a port of its own.
    pub aliases: Vec<Alias>,
}

/// A URL path served from a directory outside the document root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// The URL path, with a leading slash, e.g. `/phpmyadmin`.
    pub path: String,
    /// The directory that serves it.
    pub directory: PathBuf,
}

/// How PHP is wired into Apache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpIntegration {
    /// PHP version, for diagnostics.
    pub version: String,
    /// The `mod_php` module to load.
    pub module: PathBuf,
    /// Directory holding `php.ini` (`PHPIniDir`).
    pub ini_dir: PathBuf,
}

/// Finds Apache: a Lambo-managed installation first, then the system one.
///
/// Preferring the managed runtime keeps behaviour identical on every machine;
/// falling back to a system Apache means a developer who already has one does
/// not have to download a second copy.
pub fn discover(paths: &Paths, os: Os) -> Option<Apache> {
    if let Ok(Some(runtime)) = runtime::active(paths, RuntimeKind::Apache) {
        if let Some(apache) = from_runtime(runtime, os) {
            return Some(apache);
        }
    }
    if let Ok(runtimes) = runtime::installed(paths, RuntimeKind::Apache) {
        if let Some(runtime) = runtimes.into_iter().find(|runtime| runtime.is_complete(os)) {
            if let Some(apache) = from_runtime(runtime, os) {
                return Some(apache);
            }
        }
    }
    for program in ["httpd", "apache2"] {
        if let Some(executable) = process::find_program(program, os) {
            let server_root = executable
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/usr"));
            return Some(Apache {
                runtime: None,
                executable,
                server_root,
            });
        }
    }
    None
}

/// Builds an [`Apache`] from a managed runtime.
fn from_runtime(runtime: InstalledRuntime, os: Os) -> Option<Apache> {
    let executable = runtime.server_executable(os)?;
    let server_root = runtime.path.clone();
    Some(Apache {
        runtime: Some(runtime),
        executable,
        server_root,
    })
}

/// Installs Apache from the catalogue.
pub fn install(
    paths: &Paths,
    catalog: &Catalog,
    spec: &VersionSpec,
    platform: crate::platform::Platform,
    downloader: &dyn Downloader,
    sources: &crate::config::SourcesConfig,
) -> Result<Apache> {
    let release = catalog.find(Family::Apache, spec, &platform.key()).ok_or_else(|| {
        Error::InvalidInput(format!(
            "no Apache release for {platform} in the catalogue. On Linux and macOS, install Apache \
             with your package manager (`apt install apache2`, `brew install httpd`), or switch to \
             PHP's built-in server: {}",
            crate::config::SWITCH_TO_PHP_SERVER
        ))
    })?;
    install_release(paths, &release, downloader, platform.os, sources)
}

/// Installs one specific catalogue release.
pub fn install_release(
    paths: &Paths,
    release: &Release,
    downloader: &dyn Downloader,
    os: Os,
    sources: &crate::config::SourcesConfig,
) -> Result<Apache> {
    let version = semver::Version::parse(&release.version).map_err(|_| {
        Error::InvalidInput(format!("`{}` is not a valid version", release.version))
    })?;

    let resolved = crate::sources::resolve(
        Family::Apache,
        release,
        sources,
        paths,
        release.from_override,
    );
    let verified = crate::download::download_verified(downloader, &resolved.artifact, paths)
        .map_err(|error| {
            error.identify(Family::Apache, release).with_hint(format!(
                "Pin the digest in the catalogue override at `{}`",
                paths.catalogs_dir().display()
            ))
        })?;
    let final_dir = paths.runtime_version_dir(RuntimeKind::Apache, &release.version);
    let staging = final_dir.with_file_name(format!("{}.installing", release.version));

    let _ = fs::remove_dir_all(&staging);
    let extraction = match crate::archive::extract(&verified.path, &staging) {
        Ok(extraction) => extraction,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Some(wrapper) = extraction.single_top_level_dir() {
        crate::fsx::move_children(&staging.join(wrapper), &staging)?;
    }

    crate::runtime::write_manifest(
        RuntimeKind::Apache,
        &release.version,
        &release.platform,
        &resolved,
        &verified.sha256,
        release.executable.as_deref(),
        &staging,
    )?;

    crate::runtime::promote(&staging, &final_dir)?;

    let runtime = InstalledRuntime {
        kind: RuntimeKind::Apache,
        name: release.version.clone(),
        version,
        path: final_dir,
    };
    if !runtime.is_complete(os) {
        let _ = fs::remove_dir_all(&runtime.path);
        return Err(Error::RuntimeNotInstalled {
            kind: "Apache",
            name: release.version.clone(),
            path: runtime.path,
        });
    }
    runtime::set_active(paths, RuntimeKind::Apache, &release.version)?;

    from_runtime(runtime, os).ok_or(Error::RuntimeMissing {
        kind: "Apache",
        command: "lambo up",
    })
}

/// Locates the `mod_php` module of a PHP runtime.
///
/// The file name carries the PHP major version (`php8apache2_4.dll`), so it is
/// searched for rather than assumed.
pub fn php_module(php_runtime: &InstalledRuntime, os: Os) -> Option<PhpIntegration> {
    if os.is_windows() {
        let candidates: Vec<PathBuf> = fs::read_dir(&php_runtime.path)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_ascii_lowercase());
                matches!(name, Some(name) if name.starts_with("php")
                    && name.contains("apache2_4")
                    && name.ends_with(".dll"))
            })
            .collect();
        let module = candidates.into_iter().next()?;
        Some(PhpIntegration {
            version: php_runtime.version.to_string(),
            module,
            ini_dir: php_runtime.path.clone(),
        })
    } else {
        // On Unix, mod_php is not distributed with PHP builds; PHP-FPM behind
        // proxy_fcgi is the supported route (docs/roadmap.md). Until that
        // lands, `server.kind: php` serves Unix projects.
        None
    }
}

/// Generates `httpd.conf` for a plan and returns its path.
///
/// The file is complete: Lambo never includes a distribution's default
/// configuration, so nothing outside the Lambo home can change how the project
/// is served.
pub fn write_config(paths: &Paths, plan: &Plan, os: Os) -> Result<PathBuf> {
    let target = paths.apache_config_file();
    fs::create_dir_all(paths.apache_config_dir())
        .map_err(|source| Error::io(paths.apache_config_dir(), source))?;
    fs::create_dir_all(paths.apache_run_dir())
        .map_err(|source| Error::io(paths.apache_run_dir(), source))?;

    let error_log = paths::to_forward_slashes(&crate::logs::apache(paths));
    let access_log = paths::to_forward_slashes(&crate::logs::file(
        paths,
        crate::logs::Group::Apache,
        "access.log",
    ));
    let pid_file = paths::to_forward_slashes(&paths.apache_run_dir().join("httpd.pid"));
    let scoreboard = paths::to_forward_slashes(&paths.apache_run_dir().join("scoreboard"));
    let server_root = quote(paths::to_forward_slashes(&plan.apache.server_root));
    let document_root = quote(paths::to_forward_slashes(&plan.document_root));

    let mut config = String::new();
    config.push_str("# Generated by Lambo PHP - do not edit.\n");
    config.push_str("# `lambo server start` rewrites this file from lambo.yml.\n\n");
    config.push_str(&format!("ServerRoot {server_root}\n"));
    config.push_str(&format!("ServerName localhost:{}\n", plan.port));
    config.push_str(&format!("Listen {}\n", plan.port));
    config.push_str(&format!("PidFile {}\n", quote(pid_file)));
    // On Windows the scoreboard lives next to the PID file; leaving it at the
    // compiled-in default makes a second Apache instance fail to start.
    config.push_str(&format!("ScoreBoardFile {}\n", quote(scoreboard)));
    config.push_str("ServerTokens Prod\n");
    config.push_str("ServerSignature Off\n\n");

    // Modules: the minimum that serves PHP over HTTP. `mod_unixd` exists only
    // on Unix, so it is left out entirely on Windows rather than loaded and
    // allowed to stop Apache from starting.
    let mut modules = vec![
        ("mpm_module", mpm_module_name(os)),
        ("authz_core_module", "mod_authz_core"),
        ("dir_module", "mod_dir"),
        ("mime_module", "mod_mime"),
        ("log_config_module", "mod_log_config"),
    ];
    // `Alias` needs mod_alias. Loaded only when something is aliased, so a
    // project with no mounted application does not load a module it cannot use.
    if !plan.aliases.is_empty() {
        modules.push(("alias_module", "mod_alias"));
    }
    if !os.is_windows() {
        modules.push(("unixd_module", "mod_unixd"));
    }
    for (name, file_stem) in modules {
        config.push_str(&module_line(name, file_stem, &plan.apache));
    }
    config.push('\n');

    if let Some(php) = &plan.php {
        config.push_str(&format!(
            "LoadModule php_module {}\n",
            quote(paths::to_forward_slashes(&php.module))
        ));
        config.push_str(&format!(
            "PHPIniDir {}\n",
            quote(paths::to_forward_slashes(&php.ini_dir))
        ));
        config.push('\n');
    }

    config.push_str(&format!("ErrorLog {}\n", quote(error_log)));
    config.push_str(&format!("CustomLog {} combined\n", quote(access_log)));
    config.push_str("LogLevel warn\n\n");

    let mime_types = plan.apache.server_root.join("conf").join("mime.types");
    if mime_types.is_file() {
        config.push_str(&format!(
            "TypesConfig {}\n\n",
            quote(paths::to_forward_slashes(&mime_types))
        ));
    }

    config.push_str(&format!("DocumentRoot {document_root}\n"));
    config.push_str(&format!(
        "DirectoryIndex {}\n",
        plan.directory_index.join(" ")
    ));
    config.push_str(&format!("<Directory {document_root}>\n"));
    config.push_str("    Options Indexes FollowSymLinks\n");
    config.push_str(&format!(
        "    AllowOverride {}\n",
        if plan.allow_override { "All" } else { "None" }
    ));
    config.push_str("    Require all granted\n");
    config.push_str("</Directory>\n\n");

    if plan.php.is_some() {
        config.push_str("<FilesMatch \"\\.php$\">\n");
        config.push_str("    SetHandler application/x-httpd-php\n");
        config.push_str("</FilesMatch>\n");
    }

    // Applications Lambo manages, mounted into the same site. `/phpmyadmin`
    // lives here rather than on a port of its own, which is what makes the
    // user-facing URL `http://localhost/phpmyadmin`.
    //
    // Each alias needs its own <Directory>: `Require all granted` on the
    // document root does not extend to a directory outside it, and without it
    // Apache answers 403 for every request to the mounted path.
    for alias in &plan.aliases {
        let directory = paths::to_forward_slashes(&alias.directory);
        let quoted = quote(directory.clone());
        config.push_str(&format!("\nAlias {} {quoted}\n", alias.path));
        config.push_str(&format!("<Directory {quoted}>\n"));
        config.push_str("    Options FollowSymLinks\n");
        // Lambo generates no `.htaccess` for a managed application, and
        // honouring one that appeared there would let a stray file change how
        // it is served.
        config.push_str("    AllowOverride None\n");
        config.push_str("    Require all granted\n");
        if plan.php.is_some() {
            config.push_str("    <FilesMatch \"\\.php$\">\n");
            config.push_str("        SetHandler application/x-httpd-php\n");
            config.push_str("    </FilesMatch>\n");
        }
        config.push_str(&format!(
            "    DirectoryIndex {}\n",
            plan.directory_index.join(" ")
        ));
        config.push_str("</Directory>\n");
    }

    crate::fsx::write_atomic(&target, &config)?;
    Ok(target)
}

/// Quotes a configuration value.
///
/// Apache accepts quoted values everywhere a path appears, and quoting is
/// mandatory the moment a Windows profile directory contains a space
/// (`C:/Users/Jane Doe/...`), so Lambo always quotes.
fn quote(value: String) -> String {
    format!("\"{value}\"")
}

/// The MPM module name for the platform.
fn mpm_module_name(os: Os) -> &'static str {
    // Windows has exactly one MPM; on Unix the event MPM is the default build.
    if os.is_windows() {
        "mod_mpm_winnt"
    } else {
        "mod_mpm_event"
    }
}

/// A `LoadModule` line pointing into Apache's `modules/` directory.
///
/// Apache modules are `.so` files on every platform, Windows included.
fn module_line(name: &str, file_stem: &str, apache: &Apache) -> String {
    let module = apache
        .server_root
        .join("modules")
        .join(format!("{file_stem}.so"));
    format!(
        "LoadModule {name} {}\n",
        quote(paths::to_forward_slashes(&module))
    )
}

/// Validates a generated configuration with `httpd -t`.
///
/// Returns Apache's own output, which is what the user needs to see when
/// something is wrong.
pub fn validate(paths: &Paths, apache: &Apache, os: Os) -> Result<String> {
    let spec = ProcessSpec::new(&apache.executable, "apache")
        .args(["-t", "-f"])
        .arg(paths.apache_config_file().display().to_string())
        .stdout(Output::Inherit)
        .stderr(Output::Inherit);
    let output = process::run(&spec, os)?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(Error::ServiceFailed {
            service: "Apache".to_owned(),
            reason: "the generated configuration failed `httpd -t`".to_owned(),
            causes: vec![
                "the PHP module does not match the Apache build".to_owned(),
                "a module file is missing from the Apache installation".to_owned(),
                format!("details: {}", text.trim()),
            ],
            hint: Some("lambo doctor".to_owned()),
        });
    }
    Ok(text)
}

/// The command line used to run Apache.
///
/// Kept separate from [`start`] so the exact invocation is inspectable in
/// tests and in `lambo doctor` output: `-f <config>` is what makes the
/// generated configuration authoritative.
pub fn command_spec(plan: &Plan, config_file: &Path, log: &Path) -> ProcessSpec {
    ProcessSpec::new(&plan.apache.executable, "apache")
        .arg("-f")
        .arg(config_file.display().to_string())
        .cwd(&plan.apache.server_root)
        .log_to(log)
        .detached()
}

/// Whether the configured port is serving.
pub fn is_serving(port: u16) -> bool {
    crate::http::is_up(&crate::naming::local_url(port))
}

/// The server kind a platform should default to.
///
/// Apache with `mod_php` is the Windows story. On Linux and macOS Lambo does
/// not ship Apache, so PHP's own server is the default unless the user has a
/// system Apache installed.
pub fn default_server_kind(os: Os, apache_available: bool) -> ServerKind {
    match (os.is_windows(), apache_available) {
        // Windows ships with nothing usable, so Lambo installs Apache.
        (true, _) => ServerKind::Apache,
        // A Linux/macOS user who already has Apache keeps using it.
        (false, true) => ServerKind::Apache,
        (false, false) => ServerKind::Php,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::LocalDownloader;
    use crate::testutil::{self, TempDir};

    /// A fake Apache installation with the layout of a real one.
    fn fake_apache(paths: &Paths, version: &str, os: Os) -> Apache {
        testutil::install_fake_runtime(paths, RuntimeKind::Apache, version, os);
        let runtime = runtime::installed(paths, RuntimeKind::Apache)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        // A real installation has a modules directory and a mime.types file.
        fs::create_dir_all(runtime.path.join("modules")).unwrap();
        fs::create_dir_all(runtime.path.join("conf")).unwrap();
        fs::write(runtime.path.join("conf/mime.types"), "text/html html\n").unwrap();
        for module in [
            mpm_module_name(os),
            "mod_authz_core",
            "mod_dir",
            "mod_mime",
            "mod_log_config",
        ] {
            fs::write(
                runtime.path.join("modules").join(format!("{module}.so")),
                b"",
            )
            .unwrap();
        }
        from_runtime(runtime, os).unwrap()
    }

    fn plan(paths: &Paths, apache: Apache) -> Plan {
        Plan {
            apache,
            port: 8080,
            document_root: paths.projects_dir().join("shop").join("public"),
            project_name: "shop".to_owned(),
            php: None,
            allow_override: true,
            directory_index: vec!["index.php".to_owned(), "index.html".to_owned()],
            aliases: Vec::new(),
        }
    }

    #[test]
    fn a_mounted_application_is_aliased_into_the_site() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::Linux);
        let mut plan = plan(&paths, apache);
        plan.aliases = vec![Alias {
            path: "/phpmyadmin".to_owned(),
            directory: paths.dbui_dir().join("phpmyadmin"),
        }];

        let path = write_config(&paths, &plan, Os::Linux).unwrap();
        let config = fs::read_to_string(&path).unwrap();

        // This is what makes `http://localhost/phpmyadmin` work.
        assert!(
            config.contains("Alias /phpmyadmin "),
            "the manager must be aliased into the site: {config}"
        );
        // `Alias` is provided by mod_alias; without it Apache refuses to start.
        assert!(
            config.contains("mod_alias"),
            "mod_alias must be loaded for Alias to work: {config}"
        );
        // `Require all granted` on the document root does not reach a directory
        // outside it. Omitting this makes every request to the alias a 403.
        //
        // Locate the block by its opening tag rather than by searching for the
        // word "phpmyadmin": the Alias line also contains it, and a naive split
        // matches the document root's block instead.
        let alias_directory = format!(
            "<Directory \"{}\">",
            crate::paths::to_forward_slashes(&paths.dbui_dir().join("phpmyadmin"))
        );
        let start = config
            .find(&alias_directory)
            .unwrap_or_else(|| panic!("the alias needs its own <Directory> block: {config}"));
        let alias_block = &config[start..];
        let alias_block = &alias_block[..alias_block.find("</Directory>").expect("block closes")];

        assert!(
            alias_block.contains("Require all granted"),
            "the aliased directory must grant access: {alias_block}"
        );
        assert!(
            alias_block.contains("AllowOverride None"),
            "a managed application must not honour a stray .htaccess: {alias_block}"
        );
    }

    #[test]
    fn mod_alias_is_not_loaded_when_nothing_is_mounted() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::Linux);
        let plan = plan(&paths, apache);
        assert!(plan.aliases.is_empty());

        let path = write_config(&paths, &plan, Os::Linux).unwrap();
        let config = fs::read_to_string(&path).unwrap();

        assert!(
            !config.contains("mod_alias"),
            "a project with no mounted application must not load mod_alias: {config}"
        );
        assert!(!config.contains("Alias "), "{config}");
    }

    #[test]
    fn the_generated_configuration_serves_the_project() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::Windows);
        let mut plan = plan(&paths, apache);
        plan.php = Some(PhpIntegration {
            version: "8.4.2".to_owned(),
            module: paths.php_dir().join("8.4.2").join("php8apache2_4.dll"),
            ini_dir: paths.php_dir().join("8.4.2"),
        });

        let path = write_config(&paths, &plan, Os::Windows).unwrap();
        let config = fs::read_to_string(&path).unwrap();

        assert!(config.starts_with("# Generated by Lambo PHP"));
        assert!(config.contains("Listen 8080"));
        assert!(config.contains("ServerName localhost:8080"));
        assert!(config.contains("DocumentRoot"), "{config}");
        assert!(config.contains("AllowOverride All"));
        assert!(config.contains("DirectoryIndex index.php index.html"));
        assert!(config.contains("SetHandler application/x-httpd-php"));
        assert!(config.contains("PHPIniDir"));
        assert!(config.contains("php8apache2_4.dll"));
        assert!(
            config.contains("mod_mpm_winnt"),
            "Windows needs the WinNT MPM: {config}"
        );
        assert!(
            !config.contains("mod_unixd"),
            "mod_unixd does not exist on Windows: {config}"
        );

        // Apache's configuration parser treats a backslash as an escape
        // character, so no path may contain one. The one legitimate backslash
        // is the `\.php$` regular expression in <FilesMatch>, which is why
        // that line is excluded rather than the check being dropped.
        let path_lines: Vec<&str> = config
            .lines()
            .filter(|line| !line.contains("FilesMatch"))
            .collect();
        for line in &path_lines {
            assert!(
                !line.contains('\\'),
                "paths must use forward slashes: {line}"
            );
        }
        assert!(
            config.contains("PidFile"),
            "the PID file must live in the Lambo home"
        );
        assert!(
            config.contains("ScoreBoardFile"),
            "the scoreboard must not use the default"
        );
        assert!(
            config.contains("logs/apache/httpd.log") || config.contains("logs/apache/access.log")
        );
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::Windows);
        let mut plan = plan(&paths, apache);
        plan.document_root = PathBuf::from(r"C:\Lambo\My Projects\shop\public");

        write_config(&paths, &plan, Os::Windows).unwrap();
        let config = fs::read_to_string(paths.apache_config_file()).unwrap();

        assert!(
            config.contains("\"C:/Lambo/My Projects/shop/public\""),
            "spaces must be quoted:\n{config}"
        );
    }

    #[test]
    fn unix_configuration_differs_where_it_must() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::Linux);
        let plan = plan(&paths, apache);

        write_config(&paths, &plan, Os::Linux).unwrap();
        let config = fs::read_to_string(paths.apache_config_file()).unwrap();

        assert!(
            config.contains("mod_mpm_event"),
            "Unix uses the event MPM: {config}"
        );
        assert!(!config.contains("SetHandler application/x-httpd-php"));
        assert!(config.contains("Require all granted"));
    }

    #[test]
    fn discovery_prefers_the_managed_runtime() {
        let temp = TempDir::new();
        let paths = temp.home();
        assert!(discover(&paths, Os::Linux).is_none());

        testutil::install_fake_runtime(&paths, RuntimeKind::Apache, "2.4.62", Os::Linux);
        runtime::set_active(&paths, RuntimeKind::Apache, "2.4.62").unwrap();

        let apache = discover(&paths, Os::Linux).unwrap();
        assert!(apache.runtime.is_some());
        assert!(apache.executable.starts_with(paths.root()));
        assert_eq!(
            apache.server_root,
            paths.runtime_version_dir(RuntimeKind::Apache, "2.4.62")
        );
        assert!(apache.describe().contains("managed"));
    }

    #[test]
    fn discovery_reports_nothing_when_apache_is_absent() {
        let temp = TempDir::new();
        let paths = temp.home();
        // A brand-new Lambo home on a machine without Apache.
        assert!(discover(&paths, Os::host()).is_none());
    }

    #[test]
    fn the_command_line_makes_the_generated_config_authoritative() {
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::host());
        let plan = plan(&paths, apache);
        let log = crate::logs::apache(&paths);

        let spec = command_spec(&plan, &paths.apache_config_file(), &log);
        let rendered = spec.render();

        assert!(rendered.contains("-f"), "{rendered}");
        assert!(
            rendered.contains(&paths.apache_config_file().display().to_string()),
            "Apache must run with Lambo's generated config: {rendered}"
        );
        assert!(spec.detached, "the service must outlive the CLI");
        assert_eq!(spec.cwd.as_deref(), Some(plan.apache.server_root.as_path()));
        assert_eq!(spec.name, "apache");
    }

    #[test]
    fn the_generated_configuration_is_written_where_the_editor_opens_it() {
        // The service itself is started by the engine, from the installation's
        // `httpd.conf`; this file is what the config editor shows and what
        // `apache::validate` checks, so it has to land at the path both of them
        // name - and be complete enough to serve the project.
        let temp = TempDir::new();
        let paths = temp.home();
        let apache = fake_apache(&paths, "2.4.62", Os::host());
        let plan = plan(&paths, apache);

        let written = write_config(&paths, &plan, Os::host()).unwrap();
        assert_eq!(written, paths.apache_config_file());

        let config = fs::read_to_string(&written).unwrap();
        assert!(config.starts_with("# Generated by Lambo PHP"), "{config}");
        assert!(config.contains("Listen "), "{config}");
        assert!(config.contains("DocumentRoot"), "{config}");
    }
    #[test]
    fn installing_a_verified_archive_yields_a_usable_apache() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = temp.join("httpd-2.4.62.zip");
        testutil::write_zip(
            &archive,
            &[
                ("Apache24/", None),
                ("Apache24/bin/", None),
                ("Apache24/bin/httpd.exe", Some(b"MZ".as_slice())),
                ("Apache24/modules/", None),
                (
                    "Apache24/modules/mod_mpm_winnt.so",
                    Some(b"module".as_slice()),
                ),
                ("Apache24/conf/", None),
                (
                    "Apache24/conf/mime.types",
                    Some(b"text/html html".as_slice()),
                ),
            ],
        );

        let release = Release {
            version: "2.4.62".to_owned(),
            platform: "windows-x64".to_owned(),
            url: format!("file://{}", archive.display()),
            sha256: Some(crate::sha256::sha256_file(&archive).unwrap()),
            checksum_url: None,
            ..Default::default()
        };
        let apache = install_release(
            &paths,
            &release,
            &LocalDownloader,
            Os::Windows,
            &Default::default(),
        )
        .unwrap();

        assert_eq!(
            apache.server_root,
            paths.runtime_version_dir(RuntimeKind::Apache, "2.4.62")
        );
        assert!(apache.executable.ends_with("httpd.exe"));
        assert!(apache.executable.is_file());
        assert!(
            !temp.join("2.4.62.installing").exists(),
            "staging must be gone"
        );
        assert_eq!(
            runtime::active(&paths, RuntimeKind::Apache)
                .unwrap()
                .unwrap()
                .name,
            "2.4.62"
        );

        // The installed runtime can serve a generated configuration.
        let plan = plan(&paths, apache);
        let config = fs::read_to_string(write_config(&paths, &plan, Os::Windows).unwrap()).unwrap();
        assert!(config.contains("Listen 8080"));
    }

    #[test]
    fn an_archive_without_httpd_is_rejected() {
        let temp = TempDir::new();
        let paths = temp.home();
        let archive = temp.join("httpd-2.4.62.zip");
        testutil::write_zip(
            &archive,
            &[("readme.txt", Some(b"nothing here".as_slice()))],
        );

        let release = Release {
            version: "2.4.62".to_owned(),
            platform: "windows-x64".to_owned(),
            url: format!("file://{}", archive.display()),
            sha256: Some(crate::sha256::sha256_file(&archive).unwrap()),
            checksum_url: None,
            ..Default::default()
        };
        let error = install_release(
            &paths,
            &release,
            &LocalDownloader,
            Os::Windows,
            &Default::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::RuntimeNotInstalled { .. }),
            "{error:?}"
        );
        assert!(
            runtime::installed(&paths, RuntimeKind::Apache)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_release_the_catalogue_does_not_offer_is_explained() {
        let temp = TempDir::new();
        let paths = temp.home();
        let catalog = Catalog::embedded().unwrap();
        let platform = crate::platform::Platform::new(Os::Linux, crate::platform::Arch::X86_64);
        let error = install(
            &paths,
            &catalog,
            &"2.4".parse::<VersionSpec>().unwrap(),
            platform,
            &LocalDownloader,
            &Default::default(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("no Apache release"), "{message}");
        assert!(message.contains("server.kind php"), "{message}");
    }
}
