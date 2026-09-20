//! An installation that used to belong to the previous implementation.
//!
//! Upgrading is the one path where a mistake is unrecoverable for the user, so
//! it is tested against a *verbatim* artifact rather than a shape invented
//! here: `tests/fixtures/legacy-default-config.json` is exactly what the
//! previous implementation's `DefaultConfig` + `json.MarshalIndent` write on
//! first launch, `"projects": null` and all.
//!
//! Four surfaces carry a previous installation's state, and each is walked
//! end to end:
//!
//! | Surface | What must happen |
//! |---|---|
//! | `config.json` | loads unchanged, including the `null` list Go writes |
//! | hosts file | the legacy managed block is replaced, not duplicated, and lines outside it survive |
//! | Apache include | same rule, one block either way |
//! | `downloads/` | a valid archive is adopted, so nothing is downloaded twice; the legacy sidecar is cleared, never trusted |
//!
//! Nothing here deletes or rewrites the previous implementation's files. What
//! it asserts is what item 4 of the Phase-9 brief asks for, one assertion per
//! sentence: no duplicated hosts or vhosts, no re-download of a valid archive,
//! and user content outside a managed block left exactly as it was.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lambo_core::download::Downloader;
use lambo_core::download_cache::{DownloadCache, LEGACY_SIDECAR_FILE, SIDECAR_FILE};
use lambo_core::error::Result;
use lambo_core::panel::{CONFIG_FILE, PanelConfig};
use lambo_core::paths::Paths;
use lambo_core::vhost::{
    self, APACHE_MARKER_BEGIN, HOSTS_MARKER_BEGIN, LEGACY_APACHE_MARKER_BEGIN,
    LEGACY_HOSTS_MARKER_BEGIN,
};

/// Byte-for-byte what the previous implementation writes on first launch.
const LEGACY_DEFAULT_CONFIG: &str = include_str!("fixtures/legacy-default-config.json");

/// A temporary directory that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("lambo-legacy-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create the temporary directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A downloader that must never be called.
struct ForbiddenDownloader {
    calls: Arc<AtomicUsize>,
}

impl Downloader for ForbiddenDownloader {
    fn fetch(&self, url: &str, _destination: &Path) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("a valid archive in the cache must not be downloaded again: {url}");
    }
}

fn nop_log() -> lambo_core::logs::LogFn {
    Arc::new(|_line: &str| {})
}

/// The installation as the previous implementation left it.
fn legacy_installation(temp: &TempDir) -> Paths {
    let paths = Paths::from_root(temp.path());
    fs::write(paths.root().join(CONFIG_FILE), LEGACY_DEFAULT_CONFIG).unwrap();
    paths
}

#[test]
fn the_previous_implementations_state_file_opens_here() {
    let temp = TempDir::new();
    let paths = legacy_installation(&temp);

    let config = PanelConfig::load(paths.root()).unwrap();
    assert_eq!(config.services.len(), 30, "every service it ships survives");
    assert_eq!(config.projects.len(), 0, "`projects: null` means none");
    assert_eq!(config.vhosts.len(), 1, "its shipped virtual host");
    assert_eq!(config.settings.active_web_server, "Apache");

    // Lambo's own configuration is a different file, so the two coexist and
    // the previous implementation's file is untouched by reading it.
    let text = fs::read_to_string(paths.root().join(CONFIG_FILE)).unwrap();
    assert_eq!(text, LEGACY_DEFAULT_CONFIG, "reading must not rewrite it");
    assert!(!paths.config_file().exists(), "lambo.yml was not created");
}

#[test]
fn the_legacy_managed_blocks_are_replaced_not_duplicated() {
    let temp = TempDir::new();
    let paths = legacy_installation(&temp);

    // A hosts file as the previous implementation would leave it: the user's
    // own lines, then its managed block.
    let hosts = paths.root().join("hosts");
    fs::write(
        &hosts,
        format!(
            "127.0.0.1 localhost\n\
             10.0.0.5 nas\n\
             {LEGACY_HOSTS_MARKER_BEGIN}\n\
             127.0.0.1 myapp.test\n\
             # <<< GoAMPP managed hosts END >>>\n"
        ),
    )
    .unwrap();

    // The Apache include, likewise.
    let apache = paths.root().join("conf/apache/vhosts.conf");
    fs::create_dir_all(apache.parent().unwrap()).unwrap();
    fs::write(
        &apache,
        format!(
            "# written by the previous implementation\n\
             {LEGACY_APACHE_MARKER_BEGIN}\n\
             <VirtualHost *:80>\n  ServerName myapp.test\n</VirtualHost>\n\
             # <<< GoAMPP managed Apache vhosts END >>>\n"
        ),
    )
    .unwrap();

    // The state refers to those two files, and enables its own virtual host -
    // which is what makes it appear in both.
    let mut config = PanelConfig::load(paths.root()).unwrap();
    config.settings.hosts_file = hosts.to_string_lossy().into_owned();
    config.settings.apache_vhosts_include = apache.to_string_lossy().into_owned();
    config.vhosts[0].enabled = true;
    vhost::apply(paths.root(), &config).unwrap();

    let hosts_text = fs::read_to_string(&hosts).unwrap();
    assert!(hosts_text.contains("127.0.0.1 localhost"), "{hosts_text}");
    assert!(hosts_text.contains("10.0.0.5 nas"), "{hosts_text}");
    // The managed block carries both addresses for the domain (§4's contract),
    // so the absence of duplication is the count being two and not three: the
    // legacy block's line was replaced, not added to.
    assert_eq!(
        hosts_text.matches("127.0.0.1 myapp.test").count(),
        1,
        "one IPv4 line, not the legacy one plus ours: {hosts_text}"
    );
    assert_eq!(
        hosts_text.matches("myapp.test").count(),
        2,
        "the domain is served on both addresses and nothing is doubled: {hosts_text}"
    );
    assert_eq!(
        hosts_text.matches(HOSTS_MARKER_BEGIN).count(),
        1,
        "exactly one managed block: {hosts_text}"
    );
    assert!(
        !hosts_text.contains("GoAMPP"),
        "the legacy block is gone, not kept alongside: {hosts_text}"
    );

    let apache_text = fs::read_to_string(&apache).unwrap();
    assert_eq!(apache_text.matches(APACHE_MARKER_BEGIN).count(), 1);
    assert!(!apache_text.contains(LEGACY_APACHE_MARKER_BEGIN));
    assert!(
        apache_text.contains("ServerName myapp.test"),
        "the host is served: {apache_text}"
    );

    // And applying a second time is idempotent - the failure mode this guards
    // against is a block that grows on every launch.
    vhost::apply(paths.root(), &config).unwrap();
    let again = fs::read_to_string(&hosts).unwrap();
    assert_eq!(again, hosts_text);
    assert_eq!(again.matches("myapp.test").count(), 2, "{again}");
}

#[test]
fn a_valid_legacy_archive_is_adopted_instead_of_downloaded_again() {
    let temp = TempDir::new();
    let paths = legacy_installation(&temp);

    let downloads = paths.root().join("downloads");
    fs::create_dir_all(&downloads).unwrap();

    // A usable archive, and a poisoned one, both recorded as verified by the
    // previous implementation.
    let good = downloads.join("Apache.zip");
    fs::write(&good, b"PK\x03\x04and then some").unwrap();
    fs::write(downloads.join("Nginx.zip"), b"<html>404</html>").unwrap();
    fs::write(
        downloads.join(LEGACY_SIDECAR_FILE),
        "Apache.zip\nNginx.zip\n",
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let mut cache = DownloadCache::new(
        paths.root(),
        nop_log(),
        Box::new(ForbiddenDownloader {
            calls: Arc::clone(&calls),
        }),
    );

    // The usable one is a hit: validated by content, then adopted here.
    assert!(
        cache.ensure("Apache.zip").expect("ensure must succeed"),
        "a valid archive in the previous download cache must not be fetched again"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing was downloaded");
    assert!(cache.adopted().any(|name| name == "Apache.zip"));

    // The poisoned one is not: the legacy record is not proof of anything.
    assert!(!cache.ensure("Nginx.zip").expect("ensure must succeed"));
    assert!(
        !cache.adopted().any(|name| name == "Nginx.zip"),
        "another program's sidecar line must not make a broken file valid"
    );

    // And the legacy sidecar itself is cleared rather than left to be read
    // again, while the new one records what was actually verified.
    assert!(
        !downloads.join(LEGACY_SIDECAR_FILE).exists(),
        "the previous implementation's sidecar is cleared"
    );
    assert!(downloads.join(SIDECAR_FILE).is_file());
}

#[test]
fn the_installations_own_configuration_is_not_required_to_be_rewritten() {
    // The migration brief's second rule: a new installation is Lambo-only. An
    // upgraded one keeps the state file it has, and nothing creates the other
    // product's legacy paths.
    let temp = TempDir::new();
    let paths = legacy_installation(&temp);
    let config = PanelConfig::load(paths.root()).unwrap();
    config.save(paths.root()).unwrap();

    // Saving it here produces a file that still loads here, and that a
    // configuration written by king-PHP - the other legacy shape - is
    // converted by `lambo migrate` rather than by this path.
    let reloaded = PanelConfig::load(paths.root()).unwrap();
    assert_eq!(reloaded, config);

    let text = fs::read_to_string(paths.root().join(CONFIG_FILE)).unwrap();
    assert!(
        !text.contains("null"),
        "an empty list is written as []: {text}"
    );

    // The other legacy shape - king-PHP's `config/king.yml` - is what
    // `lambo migrate` converts; this path must not invent one.
    assert!(!paths.root().join("config").join("king.yml").exists());
}
