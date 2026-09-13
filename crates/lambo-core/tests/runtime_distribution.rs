//! End-to-end tests for the distribution pipeline, driven by checked-in
//! artifacts.
//!
//! Most of the install tests build an archive in memory and compute its digest
//! on the spot, which proves the code works but not that the *data* is right.
//! A pinned digest in a catalogue file is only worth anything if it matches the
//! bytes it claims to describe, and nothing catches a digest that has drifted
//! from its archive until a user hits it.
//!
//! So the archives in `tests/fixtures/runtime/` are committed with their
//! digests committed beside them, and this file asserts the two agree before it
//! uses them. The digests are real SHA-256 values over real files: if an
//! archive is ever edited without the digest being updated, the first test here
//! fails and says so.
//!
//! These fixtures describe a PHP that does not exist. They are not a real PHP
//! build and must never be pointed at by `catalogs/default.json`.

use std::path::{Path, PathBuf};

use lambo_core::catalog::{Catalog, Family, Release};
use lambo_core::download::LocalDownloader;
use lambo_core::error::Error;
use lambo_core::paths::Paths;
use lambo_core::php;
use lambo_core::platform::{Arch, Os, Platform};
use lambo_core::runtime::{self, Integrity, RuntimeKind};

/// The directory holding the committed artifacts.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime")
}

/// Finds a module file one level below `directory`.
///
/// Mirrors the versioned-subdirectory layout real PHP builds use, so the tests
/// do not hard-code a build-id directory name.
fn find_module(directory: &Path, file: &str) -> Option<PathBuf> {
    if directory.join(file).is_file() {
        return Some(directory.join(file));
    }
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let candidate = entry.path().join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A temporary Lambo home that removes itself.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-dist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("failed to create the temporary directory");
        Self { path }
    }

    fn home(&self) -> Paths {
        Paths::from_root(&self.path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The fixture catalogue with `fixture://` rewritten to a local file URL.
///
/// The committed file holds no absolute paths, because they would be wrong on
/// every machine but the one that wrote them. Resolving them at test time is
/// the only way a checked-in catalogue can stay portable.
fn fixture_catalog() -> Catalog {
    let text = std::fs::read_to_string(fixtures().join("catalogue.json"))
        .expect("the fixture catalogue must be present");
    let catalog: Catalog = serde_json::from_str(&text).expect("the fixture catalogue must parse");

    let base = fixtures();
    let rewritten = catalog
        .releases(Family::Php)
        .iter()
        .map(|release| {
            let file_name = release
                .url
                .strip_prefix("fixture://")
                .expect("every fixture URL uses the fixture scheme");
            Release {
                url: format!("file://{}", base.join(file_name).display()),
                ..release.clone()
            }
        })
        .collect::<Vec<_>>();

    Catalog {
        php: rewritten,
        ..catalog
    }
}

/// The digest a file actually has, in the lowercase hex form the catalogue uses.
fn digest_of(name: &str) -> String {
    lambo_core::sha256::sha256_file(&fixtures().join(name))
        .unwrap_or_else(|error| panic!("cannot digest {name}: {error}"))
}

// ---------------------------------------------------------------------------
// The committed data agrees with itself.
// ---------------------------------------------------------------------------

#[test]
fn the_committed_digests_match_the_committed_archives() {
    let catalog = fixture_catalog();

    for release in catalog.releases(Family::Php) {
        let file_name = release.filename.clone().expect("a filename is recorded");
        let pinned = release.sha256.clone().expect("a digest is pinned");
        let actual = digest_of(&file_name);

        // The 8.3.16 entry pins a digest that does not match on purpose, to
        // exercise the rejection path. Every other entry must be exact.
        if pinned.chars().all(|c| c == '0') {
            assert_ne!(
                pinned, actual,
                "{file_name}: the deliberately wrong digest now matches, \
                 which means the fixture is no longer testing rejection"
            );
            continue;
        }
        assert_eq!(
            pinned, actual,
            "{file_name}: the catalogue pins {pinned} but the file hashes to {actual}"
        );
    }
}

#[test]
fn the_sidecar_files_agree_with_the_catalogue() {
    let catalog = fixture_catalog();

    for release in catalog.releases(Family::Php) {
        let file_name = release.filename.clone().expect("a filename is recorded");
        let sidecar = std::fs::read_to_string(fixtures().join(format!("{file_name}.sha256")))
            .unwrap_or_else(|error| panic!("missing sidecar for {file_name}: {error}"));
        // `<digest>  <name>`, the format `sha256sum` writes and Lambo parses.
        let (digest, named) = sidecar
            .split_once("  ")
            .unwrap_or_else(|| panic!("malformed sidecar for {file_name}: {sidecar:?}"));

        assert_eq!(
            named.trim(),
            file_name,
            "the sidecar names a different file"
        );
        assert_eq!(
            digest.trim(),
            digest_of(&file_name),
            "{file_name}.sha256 does not describe {file_name}"
        );
    }
}

#[test]
fn the_fixtures_are_reproducible() {
    // Both archives are written with fixed member timestamps and, for the zip,
    // stored rather than deflated entries, so their bytes - and therefore
    // their digests - cannot drift with the toolchain that regenerates them.
    let tar = digest_of("php-8.4.2-linux-x64.tar.gz");
    let zip = digest_of("php-8.4.2-windows-x64.zip");

    assert_eq!(
        tar, "8b445b4efc5bdf593c9acc97ee7f5d55d64eab348da73e91987f0696c19325ba",
        "the tar fixture changed; regenerate its digest and sidecar"
    );
    assert_eq!(
        zip, "43e261dcccdfd10f83948fa021fc067190e6e77593f517a602617d36559a02ef",
        "the zip fixture changed; regenerate its digest and sidecar"
    );
}

#[test]
fn the_fixture_catalogue_validates() {
    let catalog = fixture_catalog();
    let issues = catalog.validate();

    // The fixture declares every metadata field, so a validator regression
    // shows up here rather than in a user's override file.
    assert!(
        issues.is_empty(),
        "the fixture catalogue is the reference for a well-formed one: {issues:?}"
    );
    assert_eq!(
        catalog
            .releases(Family::Php)
            .iter()
            .filter(|r| r.resolved_format() == Some(lambo_core::catalog::ArchiveFormat::Zip))
            .count(),
        1,
        "exactly one fixture is a zip"
    );
}

// ---------------------------------------------------------------------------
// Installing from a pinned catalogue entry.
// ---------------------------------------------------------------------------

/// The 8.4.2 linux-x64 fixture entry, resolved to a local URL.
fn linux_release() -> Release {
    fixture_catalog()
        .releases(Family::Php)
        .iter()
        .find(|r| r.platform == "linux-x64" && r.version == "8.4.2")
        .cloned()
        .expect("the fixture catalogue has a linux 8.4.2 entry")
}

#[test]
fn installing_a_pinned_fixture_produces_a_runtime_with_a_manifest() {
    let temp = TempDir::new();
    let paths = temp.home();

    let installed = php::install_release(
        &paths,
        &linux_release(),
        &LocalDownloader,
        &Default::default(),
    )
    .expect("install");

    assert_eq!(installed.version.to_string(), "8.4.2");
    // The archive nests everything under one directory; the installed layout
    // must not carry that wrapper through.
    assert!(
        installed.path.join("bin/php").exists(),
        "{:?}",
        installed.path
    );
    // A real Unix PHP keeps its modules in a versioned directory below lib/;
    // the fixture models that so extension_dir's search is exercised.
    let modules = php::extension_dir(&installed, Os::Linux);
    assert!(
        modules.is_dir(),
        "extension_dir {modules:?} should be a directory"
    );
    assert!(
        find_module(&modules, "openssl.so").is_some(),
        "openssl.so should be somewhere under {modules:?}"
    );

    // The manifest is what an integrity check runs against, so it has to name
    // the digest the artifact was verified against.
    let manifest = runtime::read_manifest(&installed.path).expect("a manifest was written");
    assert_eq!(manifest.kind, "php");
    assert_eq!(manifest.version, "8.4.2");
    assert_eq!(manifest.platform, "linux-x64");
    assert_eq!(
        manifest.sha256,
        linux_release().sha256.unwrap(),
        "the manifest must record the digest it was verified against"
    );
    assert_eq!(
        runtime::verify(&installed.path, RuntimeKind::Php),
        Integrity::Ok
    );
}

#[test]
fn an_installed_fixture_reports_as_active_in_the_version_table() {
    let temp = TempDir::new();
    let paths = temp.home();
    let catalog = fixture_catalog();
    let platform = Platform::new(Os::Linux, Arch::X86_64);

    php::install_release(
        &paths,
        &linux_release(),
        &LocalDownloader,
        &Default::default(),
    )
    .expect("install");
    php::activate(
        &paths,
        &lambo_core::version::VersionSpec::Req("=8.4.2".parse().unwrap()),
    )
    .expect("activate");

    let rows = php::version_table(&paths, &catalog, platform, &Default::default()).expect("table");
    let row = rows
        .iter()
        .find(|r| r.version == "8.4.2")
        .expect("8.4.2 is listed");
    assert_eq!(row.status.as_str(), "active");

    // Now break it, and the same table has to stop claiming it works. Deleting
    // a file the install manifest recorded is the failure mode this catches:
    // the directory is still there, the binary still runs, but the runtime is
    // no longer what Lambo said it installed.
    let modules = php::extension_dir(
        &runtime::resolve(&paths, RuntimeKind::Php, &Default::default())
            .expect("a runtime is installed")
            .expect("and it is the active one"),
        Os::Linux,
    );
    let openssl = find_module(&modules, "openssl.so").expect("openssl.so is installed");
    std::fs::remove_file(openssl).unwrap();
    let rows = php::version_table(&paths, &catalog, platform, &Default::default()).expect("table");
    let row = rows.iter().find(|r| r.version == "8.4.2").unwrap();
    assert_eq!(
        row.status.as_str(),
        "corrupt",
        "a runtime missing recorded files must not read as active"
    );
}

#[test]
fn a_digest_that_does_not_match_installs_nothing() {
    let temp = TempDir::new();
    let paths = temp.home();

    // The 8.3.16 fixture entry points at the same archive with a digest of all
    // zeroes: the bytes are fine, the claim about them is not.
    let bogus = fixture_catalog()
        .releases(Family::Php)
        .iter()
        .find(|r| r.version == "8.3.16")
        .cloned()
        .expect("the fixture catalogue has an 8.3.16 entry");

    let error =
        php::install_release(&paths, &bogus, &LocalDownloader, &Default::default()).unwrap_err();
    match &error {
        Error::ChecksumMismatch { expected, .. } => {
            assert_eq!(expected, &bogus.sha256.clone().unwrap());
        }
        other => panic!("expected ChecksumMismatch, got {other}"),
    }

    // Neither the runtime nor a half-extracted directory may survive.
    assert!(
        runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty()
    );
    assert!(
        !paths
            .runtime_version_dir(RuntimeKind::Php, "8.3.16")
            .exists()
    );
    assert!(
        !paths
            .runtime_version_dir(RuntimeKind::Php, "8.3.16")
            .with_file_name("8.3.16.installing")
            .exists(),
        "a rejected install must not leave staging behind"
    );
}

#[test]
fn an_unpinned_entry_falls_back_to_the_published_sidecar() {
    let temp = TempDir::new();
    let paths = temp.home();

    // No digest in the catalogue at all. `Release::artifact()` still derives
    // `<url>.sha256`, and this fixture directory genuinely publishes that
    // sidecar, so verification is possible and the install must succeed.
    // Treating an absent catalogue digest as unverifiable would break every
    // upstream that publishes a `.sha256` next to its tarball.
    let mut release = linux_release();
    release.sha256 = None;
    release.checksum_url = None;

    let installed = php::install_release(&paths, &release, &LocalDownloader, &Default::default())
        .expect("the sidecar supplies the digest");

    // The manifest records the digest that was actually verified against,
    // which came from the sidecar rather than the catalogue.
    let manifest = runtime::read_manifest(&installed.path).expect("a manifest was written");
    assert_eq!(manifest.sha256, digest_of("php-8.4.2-linux-x64.tar.gz"));
}

#[test]
fn an_entry_with_neither_a_digest_nor_a_sidecar_fails_closed() {
    let temp = TempDir::new();
    let paths = temp.home();

    // Copy the artifact somewhere with no sidecar beside it. This is the case
    // that must refuse: there is nothing to verify the bytes against.
    let orphan = temp.path.join("orphan-php.tar.gz");
    std::fs::copy(fixtures().join("php-8.4.2-linux-x64.tar.gz"), &orphan).unwrap();

    let mut release = linux_release();
    release.url = format!("file://{}", orphan.display());
    release.sha256 = None;
    release.checksum_url = None;

    let error =
        php::install_release(&paths, &release, &LocalDownloader, &Default::default()).unwrap_err();
    match error {
        Error::VerificationUnavailable {
            family,
            version,
            platform,
            hint,
            ..
        } => {
            assert_eq!(family.as_deref(), Some("php"));
            assert_eq!(version.as_deref(), Some("8.4.2"));
            assert_eq!(platform.as_deref(), Some("linux-x64"));
            let hint = hint.expect("the PHP remedy is attached");
            assert!(hint.contains("config/catalogs/"), "{hint}");
        }
        other => panic!("expected VerificationUnavailable, got {other}"),
    }
    assert!(
        runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty(),
        "nothing may be installed from an artifact Lambo could not verify"
    );
}

#[test]
fn reinstalling_a_fixture_leaves_no_backup_behind() {
    let temp = TempDir::new();
    let paths = temp.home();
    let release = linux_release();

    php::install_release(&paths, &release, &LocalDownloader, &Default::default())
        .expect("first install");
    let first = runtime::read_manifest(&paths.runtime_version_dir(RuntimeKind::Php, "8.4.2"))
        .expect("first manifest");

    php::install_release(&paths, &release, &LocalDownloader, &Default::default())
        .expect("second install");
    let dir = paths.runtime_version_dir(RuntimeKind::Php, "8.4.2");

    assert_eq!(
        runtime::verify(&dir, RuntimeKind::Php),
        Integrity::Ok,
        "a reinstall must produce a consistent runtime"
    );
    assert!(
        !dir.with_file_name("8.4.2.replacing").exists(),
        "a completed reinstall discards the previous runtime"
    );
    assert!(
        runtime::read_manifest(&dir).is_some(),
        "the reinstall rewrote the manifest ({} -> present)",
        first.version
    );
}
