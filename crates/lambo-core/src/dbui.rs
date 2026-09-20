//! The database manager served at `lambo db open`.
//!
//! **phpMyAdmin** is the default, because it is what users arriving from XAMPP
//! expect and it is what `http://localhost/phpmyadmin` promises. Adminer remains
//! selectable via `dbui.kind` for anyone who prefers a single-file manager.
//!
//! Either way Lambo serves it with the PHP runtime it already manages
//! (`php -S 127.0.0.1:<port>`), bound to the loopback interface, so the manager
//! is reachable from this machine and from nowhere else. Under Apache it is
//! additionally aliased into the project's own site at `/phpmyadmin`, which is
//! the URL the product actually hands out.
//!
//! # Where the file comes from
//!
//! Lambo never downloads and runs code it cannot verify. A manager release is
//! fetched only when a SHA-256 digest is known for it - pinned in the release
//! catalogue, or supplied by the user:
//!
//! ```text
//! an entry in config/catalogs/dbui.json with a pinned sha256
//! ```
//!
//! With no digest available, [`install`] fails closed and says so rather than
//! fetching something unverifiable. Dropping the file into
//! `<lambo home>/dbui/index.php` by hand is always supported, and
//! [`discover`] picks it up; that is the documented path for machines with no
//! outbound access.
//!
//! # What is prefilled
//!
//! The URL Lambo opens carries the server, port, user and database name so the
//! login form is mostly filled in. The password is deliberately *not* in the
//! URL: it would land in the browser's history, in proxies and in any log that
//! records query strings.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::{Catalog, Family, Release};
use crate::download::Downloader;
use crate::error::{Error, Result};
use crate::fsx;
use crate::naming;
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::process::ProcessSpec;
use crate::runtime::InstalledRuntime;
use crate::version::VersionSpec;

/// The file name Lambo expects inside its database manager directory.
///
/// Both supported managers use `index.php` as their entry point, which is what
/// lets one install directory serve either one.
pub const ENTRY_FILE_NAME: &str = "index.php";

/// A usable database manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbUi {
    /// The manager's entry point.
    pub entry: PathBuf,
    /// The PHP runtime that serves it.
    pub php: InstalledRuntime,
}

/// The directory holding the database manager.
///
/// Deliberately not named after a manager: [`discover`] and [`is_installed`]
/// are called without the config in hand, so a kind-specific directory would
/// send them looking in the wrong place after a user switches managers.
pub fn directory(paths: &Paths) -> PathBuf {
    paths.dbui_dir()
}

/// Where the manager's entry point must live to be discovered.
pub fn entry_path(paths: &Paths) -> PathBuf {
    directory(paths).join(ENTRY_FILE_NAME)
}

/// The URL path the database manager is served at, when Apache serves it.
///
/// Mounted into the project's own site rather than given a port, so the
/// user-facing address is `http://localhost/phpmyadmin` and not
/// `http://localhost:8081`.
pub const URL_PATH: &str = "/phpmyadmin";

/// Apache aliases to mount the installed database manager into the site.
///
/// Empty when nothing is installed, which is the common case: a project with no
/// database manager gets no `mod_alias` and no extra `<Directory>`.
pub fn aliases(paths: &Paths) -> Vec<crate::apache::Alias> {
    if !is_installed(paths) {
        return Vec::new();
    }
    vec![crate::apache::Alias {
        path: URL_PATH.to_owned(),
        directory: directory(paths),
    }]
}

/// The URL of the database manager when Apache is serving the project.
///
/// `http://localhost/phpmyadmin` - same host, same port, different path. A
/// separate port would be a second thing for the user to remember and a second
/// thing for a firewall to prompt about.
pub fn url(http_port: u16) -> String {
    format!("{}{URL_PATH}", crate::naming::local_url(http_port))
}

/// Finds an installed manager: an entry point plus a PHP runtime to serve it.
pub fn discover(paths: &Paths, os: Os) -> Option<DbUi> {
    let entry = entry_path(paths);
    if !entry.is_file() {
        return None;
    }
    let php = crate::php::current(paths).ok().flatten()?;
    let _ = os;
    Some(DbUi { entry, php })
}

/// Downloads and installs the database manager.
///
/// Fails closed when no checksum is available for the release: a PHP file that
/// is about to be served on a port is exactly the kind of download that must be
/// verified.
pub fn install(
    paths: &Paths,
    catalog: &Catalog,
    spec: &VersionSpec,
    platform: Platform,
    downloader: &dyn Downloader,
    sources: &crate::config::SourcesConfig,
) -> Result<DbUi> {
    let release = catalog
        .find(Family::DbUi, spec, &platform.key())
        .ok_or_else(|| {
            Error::InvalidInput(format!(
                "no database manager release for {platform} in the catalogue"
            ))
        })?;
    install_release(paths, &release, downloader, sources)
}

/// Installs one specific catalogue release.
pub fn install_release(
    paths: &Paths,
    release: &Release,
    downloader: &dyn Downloader,
    sources: &crate::config::SourcesConfig,
) -> Result<DbUi> {
    let resolved =
        crate::sources::resolve(Family::DbUi, release, sources, paths, release.from_override);
    let downloaded = crate::download::download_verified(downloader, &resolved.artifact, paths)
        .map_err(|error| {
            error.identify(Family::DbUi, release).with_hint(format!(
                "Pin the digest: {}. Alternatively place the file yourself at `{}`.",
                crate::config::PIN_A_DIGEST,
                entry_path(paths).display()
            ))
        })?;

    let php = crate::php::current(paths)?.ok_or(Error::RuntimeMissing {
        kind: "PHP",
        command: "lambo php install",
    })?;

    let target = entry_path(paths);
    fsx::ensure_dir(&directory(paths))?;

    // phpMyAdmin is a multi-file application shipped as an archive; Adminer is a
    // single PHP file. Which one this is comes from the format the catalogue
    // declares for the release - never inferred from the file name, because a
    // mirror or a locally staged artifact may be named anything.
    match release.archive_format {
        Some(crate::catalog::ArchiveFormat::SingleFile) | None => {
            install_single_file(&downloaded.path, &target)?;
        }
        Some(_) => install_archive(&downloaded.path, paths, release)?,
    }
    let _ = fs::remove_file(&downloaded.path);

    Ok(DbUi { entry: target, php })
}

/// Puts a single-file application in place.
fn install_single_file(downloaded: &Path, target: &Path) -> Result<()> {
    fs::rename(downloaded, target)
        .map_err(|_| {
            // A cross-device rename fails; fall back to a copy.
            Error::io(
                target,
                std::io::Error::other("could not move the downloaded file into place"),
            )
        })
        .or_else(|_| {
            fs::copy(downloaded, target)
                .map(|_| ())
                .map_err(|source| Error::io(target, source))
        })
}

/// Extracts a multi-file application and puts it in place.
///
/// Extracts beside the final directory and renames, so a failed or partial
/// unpack never leaves a half-populated manager that `discover` would then
/// accept as installed.
fn install_archive(downloaded: &Path, paths: &Paths, release: &Release) -> Result<()> {
    let destination = directory(paths);
    // A sibling of the destination, never inside it: a previous version is
    // cleared by removing the destination wholesale, which would take the
    // staging tree with it and leave the rename with nothing to move.
    let staging = destination.with_file_name(format!(
        "dbui.staging-{}",
        crate::naming::slugify(&release.version)
    ));
    let _ = fs::remove_dir_all(&staging);
    fsx::ensure_dir(&staging)?;

    // The format comes from the catalogue, never from the downloaded file's
    // name: it is a cache entry and may not carry an extension at all.
    let kind = match release.archive_format {
        Some(crate::catalog::ArchiveFormat::Zip) => crate::archive::Kind::Zip,
        Some(crate::catalog::ArchiveFormat::TarGz) => crate::archive::Kind::TarGz,
        other => {
            let _ = fs::remove_dir_all(&staging);
            return Err(Error::InvalidInput(format!(
                "the catalogue declares {other:?} for {} {}, which is not an archive format Lambo can extract",
                release.version, release.platform
            )));
        }
    };
    let extracted = crate::archive::extract_kind(downloaded, &staging, kind).map_err(|error| {
        let _ = fs::remove_dir_all(&staging);
        error.identify(Family::DbUi, release)
    })?;
    if extracted.files == 0 {
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::InvalidInput(format!(
            "{} contains no files",
            release.filename.as_deref().unwrap_or("the archive")
        )));
    }

    // Releases ship inside one top-level directory (`phpMyAdmin-5.2.1-…/`).
    // Serving from the staging root would put that directory in the URL, so the
    // manager's own root is what gets moved into place. When the archive already
    // unpacked flat, the staging directory is itself that root.
    let root = match extracted.single_top_level_dir() {
        Some(name) => staging.join(name),
        None => staging.clone(),
    };

    // Replace any previous version. The old directory is removed rather than
    // merged into: a leftover file from an older release is indistinguishable
    // from a current one once both are on disk.
    let _ = fs::remove_dir_all(&destination);
    fsx::ensure_dir(destination.parent().unwrap_or(&destination))?;
    fs::rename(&root, &destination).map_err(|source| Error::io(&destination, source))?;

    // Whichever directory was the application root, the staging tree is now
    // surplus: the move took the contents out of it.
    let _ = fs::remove_dir_all(&staging);

    if !entry_path(paths).is_file() {
        return Err(Error::RuntimeNotInstalled {
            kind: "database manager",
            name: release.version.clone(),
            path: entry_path(paths),
        });
    }
    Ok(())
}

/// Everything needed to serve the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Port the manager listens on.
    pub port: u16,
    /// Port of the database it should connect to.
    pub database_port: u16,
    /// Database user to suggest.
    pub database_user: String,
    /// The PHP executable that serves it.
    pub php: PathBuf,
    /// The `PHPRC` value that makes PHP read Lambo's generated `php.ini`.
    ///
    /// `None` when the runtime has no generated configuration yet. Without
    /// this, the manager is served by a PHP that has never been told where its
    /// extensions live - no `mysqli`, no `pdo_mysql`, and a login form that
    /// cannot reach the database it is meant to manage.
    pub phprc: Option<String>,
    /// The manager's entry point.
    pub entry: PathBuf,
    /// Where the server's output goes.
    pub log: PathBuf,
    /// Which manager this plan serves (`phpmyadmin`, `adminer`).
    ///
    /// Carried so that a failure names the manager the user actually
    /// configured, rather than whichever one the code happens to mention.
    pub kind: String,
}

impl Plan {
    /// Builds a plan from a discovered manager.
    pub fn new(
        paths: &Paths,
        ui: &DbUi,
        port: u16,
        database_port: u16,
        database_user: impl Into<String>,
        os: Os,
        kind: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            kind: kind.into(),
            port,
            database_port,
            database_user: database_user.into(),
            // Resolved from the runtime, not from the executable path: the
            // configuration lives beside the runtime, and only the runtime
            // knows which platform layout it has.
            phprc: crate::php::phprc(&ui.php, os),
            php: crate::php::php_executable(&ui.php, os)?,
            entry: ui.entry.clone(),
            log: crate::logs::file(paths, crate::logs::Group::Lambo, "dbui.log"),
        })
    }

    /// The URL a browser should open.
    pub fn url(&self) -> String {
        open_url(self.port, self.database_port, None, &self.database_user)
    }
}

/// The command line that serves the manager.
///
/// `php -S` on the loopback interface only. Binding to `0.0.0.0` would publish
/// a database login form to the local network, which no default setting should
/// ever do.
pub fn command_spec(plan: &Plan) -> ProcessSpec {
    let docroot = plan
        .entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // The process carries the manager's own name - `phpmyadmin`, `adminer` -
    // because that is the name the configuration gives it and the one a line
    // about it should use.
    let mut spec = ProcessSpec::new(&plan.php, plan.kind.clone())
        .arg("-S")
        .arg(format!("127.0.0.1:{}", plan.port))
        .arg("-t")
        .arg(docroot.display().to_string())
        .cwd(&docroot)
        .log_to(&plan.log)
        .detached();
    // The manager is a PHP application, so it gets the same generated
    // configuration as everything else Lambo runs through PHP. The extensions
    // it needs to reach a database are enabled there.
    if let Some(phprc) = &plan.phprc {
        spec = spec.env("PHPRC", phprc);
    }
    spec
}

/// Builds the URL to open, prefilling what is safe to prefill.
///
/// The password is intentionally absent - see the module documentation.
pub fn open_url(port: u16, database_port: u16, database: Option<&str>, username: &str) -> String {
    let mut url = format!(
        "http://127.0.0.1:{port}/?server={}&username={username}",
        naming::database_host_and_port(database_port)
    );
    if let Some(database) = database {
        url.push_str(&format!("&db={database}"));
    }
    url
}

/// The URL to open when the manager is served through Apache's alias.
///
/// Carries the same prefilled connection details as [`open_url`], so switching
/// between the aliased and the standalone form does not change what the user
/// has to type. The password is absent in both.
pub fn open_url_at(
    http_port: u16,
    database_port: u16,
    database: Option<&str>,
    username: &str,
) -> String {
    let mut url = format!(
        "{}{URL_PATH}/?server={}&username={username}",
        crate::naming::local_url(http_port),
        crate::naming::database_host_and_port(database_port)
    );
    if let Some(database) = database {
        url.push_str(&format!("&db={database}"));
    }
    url
}

/// Whether a manager is installed and ready to serve.
pub fn is_installed(paths: &Paths) -> bool {
    entry_path(paths).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::LocalDownloader;
    use crate::runtime::{self, RuntimeKind};
    use crate::testutil::{self, TempDir};

    fn with_php(paths: &Paths, os: Os) {
        testutil::install_fake_runtime(paths, RuntimeKind::Php, "8.4.2", os);
        runtime::set_active(paths, RuntimeKind::Php, "8.4.2").unwrap();
    }

    #[test]
    fn nothing_is_discovered_before_installation() {
        let temp = TempDir::new();
        let paths = temp.home();
        with_php(&paths, Os::host());
        assert!(!is_installed(&paths));
        assert!(discover(&paths, Os::host()).is_none());
    }

    #[test]
    fn a_manually_placed_file_is_discovered() {
        let temp = TempDir::new();
        let paths = temp.home();
        with_php(&paths, Os::host());

        // No PHP means no manager, however complete the file is.
        fsx::ensure_dir(&directory(&paths)).unwrap();
        fs::write(entry_path(&paths), "<?php // adminer\n").unwrap();
        assert!(is_installed(&paths));

        let ui = discover(&paths, Os::host()).unwrap();
        assert_eq!(ui.entry, entry_path(&paths));
        assert_eq!(ui.php.name, "8.4.2");
    }

    #[test]
    fn the_server_is_bound_to_the_loopback_interface() {
        let temp = TempDir::new();
        let paths = temp.home();
        with_php(&paths, Os::host());
        fsx::ensure_dir(&directory(&paths)).unwrap();
        fs::write(entry_path(&paths), "<?php\n").unwrap();

        let ui = discover(&paths, Os::host()).unwrap();
        let plan = Plan::new(&paths, &ui, 8081, 3306, "root", Os::host(), "phpmyadmin").unwrap();
        let rendered = command_spec(&plan).render();

        assert!(rendered.contains("127.0.0.1:8081"), "{rendered}");
        assert!(
            !rendered.contains("0.0.0.0"),
            "the manager must not be published: {rendered}"
        );
        assert!(rendered.contains("-S"), "{rendered}");
        assert!(rendered.contains("-t"), "{rendered}");
    }

    #[test]
    fn the_opened_url_prefills_everything_but_the_password() {
        let url = open_url(8081, 3306, Some("shop"), "root");
        assert_eq!(
            url,
            "http://127.0.0.1:8081/?server=127.0.0.1:3306&username=root&db=shop"
        );
        assert!(!url.contains("password"), "{url}");

        let without_database = open_url(8081, 3306, None, "root");
        assert!(!without_database.contains("db="), "{without_database}");
    }

    #[test]
    fn the_aliased_url_is_absolute_and_prefills_the_same_details() {
        // Served through Apache's alias, so the address is the project's own
        // site - not a second port, and not a relative path a browser cannot
        // open on its own.
        let url = open_url_at(80, 3306, Some("shop"), "root");
        assert_eq!(
            url,
            "http://localhost/phpmyadmin/?server=127.0.0.1:3306&username=root&db=shop"
        );
        assert!(!url.contains("password"), "{url}");

        // A fallback web port still produces an absolute URL.
        let fallback = open_url_at(8080, 3306, None, "root");
        assert_eq!(
            fallback,
            "http://localhost:8080/phpmyadmin/?server=127.0.0.1:3306&username=root"
        );
        assert!(!fallback.contains("db="), "{fallback}");

        // Both forms reach the same alias path.
        assert!(url.contains(&format!("{URL_PATH}/")), "{url}");
        assert!(fallback.contains(&format!("{URL_PATH}/")), "{fallback}");
    }

    #[test]
    fn installing_without_a_checksum_fails_closed_with_a_way_forward() {
        let temp = TempDir::new();
        let paths = temp.home();
        let mut catalog = Catalog::embedded().unwrap();
        // The shipped catalogue pins a digest for this release - that is what
        // lets the pinned-release test install it. This test is about the road
        // taken when no digest is known, so it unpins the release first; the
        // user-side override catalogue is the mechanism that would do the same.
        for release in &mut catalog.dbui {
            if release.version == "5.2.3" && release.platform == "windows-x64" {
                release.sha256 = None;
            }
        }
        let platform = Platform::new(Os::Windows, crate::platform::Arch::X86_64);

        let error = install(
            &paths,
            &catalog,
            &VersionSpec::Stable,
            platform,
            &LocalDownloader,
            &Default::default(),
        )
        .unwrap_err();
        // Identity travels with the error, so a caller does not have to
        // recover it from the message.
        match &error {
            Error::VerificationUnavailable {
                family,
                version,
                platform,
                hint,
                ..
            } => {
                assert_eq!(family.as_deref(), Some("dbui"));
                // phpMyAdmin is now the default manager, so it is the release
                // that must fail closed when its digest is not pinned.
                assert_eq!(version.as_deref(), Some("5.2.3"));
                assert_eq!(platform.as_deref(), Some("windows-x64"));
                let hint = hint.as_deref().unwrap_or_default();
                // `lambo config set dbui.sha256` is not a real key; the
                // catalogue override is the mechanism that exists.
                assert!(hint.contains("config/catalogs/"), "{hint}");
                assert!(hint.contains("lambo config hash"), "{hint}");
            }
            other => panic!("expected VerificationUnavailable, got {other}"),
        }
        let message = error.to_string();
        assert!(message.contains("index.php"), "{message}");
    }

    #[test]
    fn installing_a_pinned_release_places_the_entry_point() {
        let temp = TempDir::new();
        let paths = temp.home();
        with_php(&paths, Os::host());

        let source = temp.join("adminer.php");
        fs::write(&source, "<?php // adminer 4.8.1\n").unwrap();
        let release = Release {
            version: "4.8.1".to_owned(),
            platform: "windows-x64".to_owned(),
            url: format!("file://{}", source.display()),
            sha256: Some(crate::sha256::sha256_file(&source).unwrap()),
            checksum_url: None,
            ..Default::default()
        };

        let ui = install_release(&paths, &release, &LocalDownloader, &Default::default()).unwrap();
        assert_eq!(ui.entry, entry_path(&paths));
        assert!(ui.entry.is_file());
        assert_eq!(
            fs::read_to_string(ui.entry).unwrap(),
            "<?php // adminer 4.8.1\n"
        );
        assert!(
            !paths.cache_dir().join("adminer.php").exists(),
            "the cache copy is consumed"
        );
    }

    #[test]
    fn installing_an_archive_flattens_the_release_directory() {
        let temp = TempDir::new();
        let paths = temp.home();
        with_php(&paths, Os::host());

        // phpMyAdmin ships as a zip with everything under one directory. If
        // that directory were left in place the entry point would be
        // `dbui/phpMyAdmin-5.2.3-…/index.php`, and serving `dbui/` would put
        // the release directory in the URL.
        let archive =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dbui/phpmyadmin-5.2.3.zip");
        let release = Release {
            version: "5.2.3".to_owned(),
            platform: Platform::host().key(),
            url: format!("file://{}", archive.display()),
            sha256: Some(
                "ac7ba345493a903ab5b0aa476c8e5c809e77d0f0b4caf24dc025514cec7f2a26".to_owned(),
            ),
            checksum_url: None,
            archive_format: Some(crate::catalog::ArchiveFormat::Zip),
            filename: Some("phpMyAdmin-5.2.3-all-languages.zip".to_owned()),
            ..Default::default()
        };

        let ui = install_release(&paths, &release, &LocalDownloader, &Default::default()).unwrap();

        // The entry point sits directly in the manager directory.
        assert_eq!(ui.entry, entry_path(&paths));
        assert!(ui.entry.is_file(), "index.php was not placed");
        assert!(
            !ui.entry.to_string_lossy().contains("phpMyAdmin-5.2.3"),
            "the release directory leaked into the entry path: {}",
            ui.entry.display()
        );
        // Nested application files came across too, so the whole app was
        // moved and not just its entry point.
        assert!(
            directory(&paths).join("libraries/common.inc.php").is_file(),
            "nested files were not installed"
        );
        assert!(
            directory(&paths)
                .join("themes/pmahomme/jquery/jquery-ui.css")
                .is_file(),
            "deeply nested files were not installed"
        );
        // No staging directory is left behind.
        let staging: Vec<_> = fs::read_dir(paths.dbui_dir())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name.to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(staging.is_empty(), "staging left behind: {staging:?}");
    }

    #[test]
    fn a_manager_that_is_not_installed_has_no_entry_point_to_serve() {
        // The card's Start installs a missing manager instead of running it, so
        // what a start has to know is whether the entry point is there at all.
        let temp = TempDir::new();
        let paths = temp.home();

        assert!(!is_installed(&paths));
        assert!(!entry_path(&paths).is_file());

        // A file at the entry point is what `is_installed` reports, whatever
        // wrote it - the same rule the installer's check file follows.
        fs::create_dir_all(directory(&paths)).unwrap();
        fs::write(entry_path(&paths), "<?php").unwrap();
        assert!(is_installed(&paths));
    }
}
