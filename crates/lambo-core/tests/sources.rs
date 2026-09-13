//! Where runtime artifacts come from, and what keeps that honest.
//!
//! The rules under test are the ones that matter when somebody installs a
//! runtime from somewhere other than the official host:
//!
//! - the *bytes* may come from a mirror, a local file, or an override;
//! - the *expected digest* may only come from the catalogue;
//! - a source that cannot be checked against trusted metadata fails closed.
//!
//! Nothing here invents a second downloader or a second verifier. The tests run
//! the real install path against the same committed fixture archives the rest of
//! the distribution tests use, so a mirror install and an official install are
//! proved to go through identical verification.

use std::path::{Path, PathBuf};

use lambo_core::catalog::{Catalog, Family, Release};
use lambo_core::config::SourcesConfig;
use lambo_core::download::LocalDownloader;
use lambo_core::error::Error;
use lambo_core::paths::Paths;
use lambo_core::php;
use lambo_core::runtime::RuntimeKind;
use lambo_core::sources::{self, SourceKind};

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = format!(
            "lambo-src-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }

    fn home(&self) -> Paths {
        Paths::from_root(&self.path)
    }

    fn join(&self, relative: &str) -> PathBuf {
        self.path.join(relative)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The committed fixture archives.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime")
}

/// The linux fixture archive, as a file URL and with its real digest.
fn fixture_archive() -> (String, String) {
    let path = fixtures().join("php-8.4.2-linux-x64.tar.gz");
    let digest = lambo_core::sha256::sha256_file(&path).expect("the fixture must hash");
    (format!("file://{}", path.display()), digest)
}

/// A release pointing at the real fixture archive, with a real digest.
fn release() -> Release {
    let (url, digest) = fixture_archive();
    Release {
        version: "8.4.2".to_owned(),
        platform: "linux-x64".to_owned(),
        url,
        sha256: Some(digest),
        archive_format: Some(lambo_core::catalog::ArchiveFormat::TarGz),
        filename: Some("php-8.4.2-linux-x64.tar.gz".to_owned()),
        executable: Some("bin/php".to_owned()),
        ..Default::default()
    }
}

/// Copies the fixture archive to `destination`, so a test can serve it from
/// somewhere other than the official location.
fn stage_artifact(destination: &Path) {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).expect("artifact directory");
    }
    std::fs::copy(fixtures().join("php-8.4.2-linux-x64.tar.gz"), destination)
        .expect("the fixture archive must copy");
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

#[test]
fn with_no_configuration_the_catalogue_url_is_used() {
    let temp = TempDir::new("default");
    let resolved = sources::resolve(
        Family::Php,
        &release(),
        &SourcesConfig::default(),
        &temp.home(),
        false,
    );

    assert_eq!(resolved.kind, SourceKind::Catalogue);
    assert_eq!(resolved.artifact.url, release().url);
}

#[test]
fn an_explicit_override_wins_over_a_mirror() {
    let temp = TempDir::new("precedence");
    let sources = SourcesConfig {
        mirror: "https://mirror.internal/lambo".to_owned(),
        ..Default::default()
    };
    let resolved = sources::resolve(Family::Php, &release(), &sources, &temp.home(), true);

    // The user named this artifact, so the mirror is not consulted at all.
    assert_eq!(resolved.kind, SourceKind::Override);
    assert_eq!(resolved.artifact.url, release().url);
}

#[test]
fn a_local_artifact_is_found_by_its_identity_derived_name() {
    let temp = TempDir::new("local");
    let paths = temp.home();

    // The name Lambo derives from family, version and platform - not whatever
    // the file happens to be called upstream.
    let name = sources::artifact_file_name(Family::Php, &release()).expect("a name");
    assert_eq!(name, "php-8.4.2-linux-x64.tar.gz");
    stage_artifact(&sources::artifacts_dir(&SourcesConfig::default(), &paths).join(&name));

    let resolved = sources::resolve(
        Family::Php,
        &release(),
        &SourcesConfig::default(),
        &paths,
        false,
    );

    assert_eq!(resolved.kind, SourceKind::Local);
    assert!(
        resolved.artifact.url.starts_with("file://"),
        "a local artifact is addressed as a file: {}",
        resolved.artifact.url
    );
    // The expected digest is untouched: it still comes from the catalogue.
    assert_eq!(resolved.artifact.sha256, release().sha256);
}

#[test]
fn a_mirror_rewrites_the_url_but_never_the_digest() {
    let temp = TempDir::new("mirror");
    let sources = SourcesConfig {
        mirror: "https://mirror.internal/lambo".to_owned(),
        ..Default::default()
    };
    let original = release();
    let resolved = sources::resolve(Family::Php, &original, &sources, &temp.home(), false);

    assert_eq!(resolved.kind, SourceKind::Mirror);
    assert_eq!(
        resolved.artifact.url,
        "https://mirror.internal/lambo/php-8.4.2-linux-x64.tar.gz"
    );
    // This is the whole security property: the bytes moved, the expectation did
    // not. A mirror that serves different bytes therefore fails verification.
    assert_eq!(resolved.artifact.sha256, original.sha256);
    assert_eq!(
        resolved.artifact.checksum_url,
        Some(format!("{}.sha256", original.url)),
        "the sidecar must stay pointed at the official URL, not the mirror"
    );
}

#[test]
fn a_mirror_base_with_a_trailing_slash_does_not_double_it() {
    let temp = TempDir::new("mirror-slash");
    let sources = SourcesConfig {
        mirror: "https://mirror.internal/lambo/".to_owned(),
        ..Default::default()
    };
    let resolved = sources::resolve(Family::Php, &release(), &sources, &temp.home(), false);
    assert_eq!(
        resolved.artifact.url,
        "https://mirror.internal/lambo/php-8.4.2-linux-x64.tar.gz"
    );
}

// ---------------------------------------------------------------------------
// A local artifact really installs, through the real pipeline
// ---------------------------------------------------------------------------

#[test]
fn a_local_artifact_installs_through_the_same_verification_as_an_official_one() {
    let temp = TempDir::new("local-install");
    let paths = temp.home();

    let release = release();
    let name = sources::artifact_file_name(Family::Php, &release)
        .expect("the fixture release has a usable name");
    stage_artifact(&sources::artifacts_dir(&SourcesConfig::default(), &paths).join(&name));

    let installed = php::install_release(
        &paths,
        &release,
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect("a verified local artifact installs");

    assert_eq!(installed.version.to_string(), "8.4.2");

    // And the manifest says where it came from, which is the point of recording
    // provenance at all.
    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("a manifest");
    assert_eq!(manifest.source_kind, SourceKind::Local.as_str());
    assert_eq!(manifest.sha256, release.sha256.clone().unwrap());
}

#[test]
fn a_local_artifact_with_the_wrong_contents_is_rejected() {
    let temp = TempDir::new("local-tampered");
    let paths = temp.home();

    let dir = sources::artifacts_dir(&SourcesConfig::default(), &paths);
    let name = "php-8.4.2-linux-x64.tar.gz";
    std::fs::create_dir_all(&dir).unwrap();
    // Same name, different bytes: exactly what a stale or tampered local copy
    // looks like. The catalogue digest is what catches it.
    std::fs::write(
        dir.join(name),
        b"this is not the artifact the digest describes",
    )
    .unwrap();

    let error = php::install_release(
        &paths,
        &release(),
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect_err("a local artifact must be verified like any other");

    assert!(
        matches!(error, Error::ChecksumMismatch { .. }),
        "expected a checksum mismatch, got {error:?}"
    );
    assert!(
        lambo_core::runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty(),
        "a rejected artifact must not be installed"
    );
}

// ---------------------------------------------------------------------------
// Failure closes rather than falling back
// ---------------------------------------------------------------------------

#[test]
fn a_mirror_that_serves_different_bytes_fails_verification() {
    let temp = TempDir::new("mirror-mismatch");
    let paths = temp.home();

    // A mirror holding a file with the right name and the wrong contents.
    let mirror_root = temp.join("mirror");
    std::fs::create_dir_all(&mirror_root).unwrap();
    std::fs::write(
        mirror_root.join("php-8.4.2-linux-x64.tar.gz"),
        b"a mirror serving something else entirely",
    )
    .unwrap();

    let sources = SourcesConfig {
        mirror: format!("file://{}", mirror_root.display()),
        ..Default::default()
    };
    let error = php::install_release(&paths, &release(), &LocalDownloader, &sources)
        .expect_err("a mirror may not change what the digest expects");

    assert!(
        matches!(error, Error::ChecksumMismatch { .. }),
        "expected a checksum mismatch, got {error:?}"
    );
}

#[test]
fn a_release_with_no_digest_anywhere_is_refused_even_from_a_local_file() {
    let temp = TempDir::new("no-digest");
    let paths = temp.home();

    let name = "php-8.4.2-linux-x64.tar.gz";
    stage_artifact(&sources::artifacts_dir(&SourcesConfig::default(), &paths).join(name));

    // The artifact is present and readable, and there is still no way to know
    // whether it is the right one. Being local is not a reason to skip that.
    //
    // The URL points somewhere with no `.sha256` beside it, because
    // `Release::artifact` derives the sidecar address from the URL when none is
    // declared. Clearing `checksum_url` alone would not remove the second
    // source of truth - see the next test for the case where a sidecar does
    // exist.
    let mut release = release();
    release.sha256 = None;
    release.checksum_url = None;
    release.url = format!("file://{}/nowhere/php.tar.gz", temp.path.display());

    let error = php::install_release(
        &paths,
        &release,
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect_err("no digest means no installation");
    assert!(
        matches!(error, Error::VerificationUnavailable { .. }),
        "expected verification to be unavailable, got {error:?}"
    );
    assert!(
        lambo_core::runtime::installed(&paths, RuntimeKind::Php)
            .unwrap()
            .is_empty(),
        "nothing may be installed without a digest"
    );
}

#[test]
fn an_unpinned_entry_is_still_verified_against_its_published_sidecar() {
    let temp = TempDir::new("sidecar");
    let paths = temp.home();

    // The counterpart to the test above, and the reason clearing the pinned
    // digest does not by itself make an artifact unverifiable: the fixture
    // directory publishes a `.sha256` beside the archive, and
    // `Release::artifact` derives that address from the URL.
    let name = "php-8.4.2-linux-x64.tar.gz";
    let dir = sources::artifacts_dir(&SourcesConfig::default(), &paths);
    stage_artifact(&dir.join(name));

    let mut release = release();
    release.sha256 = None;
    release.checksum_url = None;

    let installed = php::install_release(
        &paths,
        &release,
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect("a published sidecar is trusted metadata");

    // The manifest records the digest that was actually used, not the empty
    // catalogue field - otherwise it would claim nothing was verified for an
    // install that was.
    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("manifest");
    assert_eq!(manifest.sha256, fixture_archive().1);
}

// ---------------------------------------------------------------------------
// Security: paths and schemes
// ---------------------------------------------------------------------------

#[test]
fn an_artifact_name_cannot_escape_the_artifacts_directory() {
    // Version and platform come from catalogue data, which a user can override,
    // so they are the vector for a traversal. A name Lambo cannot prove is a
    // single safe component is not used at all.
    let hostile = Release {
        version: "../../etc/passwd".to_owned(),
        platform: "linux-x64".to_owned(),
        archive_format: Some(lambo_core::catalog::ArchiveFormat::TarGz),
        ..Default::default()
    };
    assert_eq!(sources::artifact_file_name(Family::Php, &hostile), None);

    let backslash = Release {
        version: r"..\..\windows\system32".to_owned(),
        platform: "windows-x64".to_owned(),
        archive_format: Some(lambo_core::catalog::ArchiveFormat::Zip),
        ..Default::default()
    };
    assert_eq!(sources::artifact_file_name(Family::Php, &backslash), None);

    // A plain name is fine.
    let plain = release();
    assert!(sources::artifact_file_name(Family::Php, &plain).is_some());
}

#[test]
fn only_https_and_file_sources_are_accepted() {
    use lambo_core::download::require_https;

    assert!(require_https("https://example.com/php.tar.gz").is_ok());
    assert!(require_https("file:///srv/artifacts/php.tar.gz").is_ok());

    // Plain HTTP can be rewritten in transit, so it is not a transport Lambo
    // will fetch a runtime over.
    assert!(require_https("http://example.com/php.tar.gz").is_err());
    // Neither is anything that would hand the URL to another program.
    assert!(require_https("ftp://example.com/php.tar.gz").is_err());
    assert!(require_https("php.tar.gz").is_err());
}

#[test]
fn a_file_url_round_trips_to_the_path_it_names() {
    let path = sources::file_url_to_path("file:///srv/Lambo Artifacts/php.tar.gz")
        .expect("a file url parses");
    assert_eq!(path, Path::new("/srv/Lambo Artifacts/php.tar.gz"));

    // Anything that is not a file URL is not a local path.
    assert_eq!(sources::file_url_to_path("https://example.com/x"), None);
}

// ---------------------------------------------------------------------------
// Paths with spaces (§20)
//
// `C:\Lambo Artifacts\php` and `D:\Downloads\Apache` are ordinary places to
// keep artifacts on Windows. None of this may require quoting, because nothing
// is passed through a shell: a path travels as one argument or as one `file://`
// URL, and a space inside it is just a character.
// ---------------------------------------------------------------------------

#[test]
fn an_artifacts_directory_with_spaces_is_used_intact() {
    let temp = TempDir::new("spaced-dir");
    let paths = temp.home();

    // A configured directory whose path contains spaces, as a Windows user
    // would have it.
    let dir = temp.join("Lambo Artifacts/php");
    let sources = SourcesConfig {
        artifacts: dir.display().to_string(),
        ..Default::default()
    };
    assert_eq!(sources::artifacts_dir(&sources, &paths), dir);

    let name = sources::artifact_file_name(Family::Php, &release()).expect("a name");
    stage_artifact(&dir.join(&name));

    let resolved = sources::resolve(Family::Php, &release(), &sources, &paths, false);
    assert_eq!(resolved.kind, SourceKind::Local);
    // The whole path survives, spaces included, as one URL.
    assert!(
        resolved.artifact.url.contains("Lambo Artifacts/php"),
        "the spaced path was altered: {}",
        resolved.artifact.url
    );
    assert!(
        !resolved.artifact.url.contains('"'),
        "a path must not need quoting: {}",
        resolved.artifact.url
    );

    // And it really installs, which is the only proof the path was usable
    // rather than merely pretty.
    let installed =
        php::install_release(&paths, &release(), &LocalDownloader, &sources).expect("install");
    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("manifest");
    assert_eq!(manifest.source_kind, SourceKind::Local.as_str());
    assert!(
        manifest.source_url.contains("Lambo Artifacts/php"),
        "the manifest must record the real source: {}",
        manifest.source_url
    );
}

#[test]
fn a_mirror_path_with_spaces_is_used_intact() {
    let temp = TempDir::new("spaced-mirror");
    let paths = temp.home();

    // A mirror on a mounted share whose path has a space in it.
    let mirror_root = temp.join("D/Downloads/Apache");
    std::fs::create_dir_all(&mirror_root).unwrap();
    std::fs::copy(
        fixtures().join("php-8.4.2-linux-x64.tar.gz"),
        mirror_root.join("php-8.4.2-linux-x64.tar.gz"),
    )
    .unwrap();

    let sources = SourcesConfig {
        mirror: format!("file://{}", mirror_root.display()),
        ..Default::default()
    };
    let installed =
        php::install_release(&paths, &release(), &LocalDownloader, &sources).expect("install");

    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("manifest");
    assert_eq!(manifest.source_kind, SourceKind::Mirror.as_str());
    assert_eq!(manifest.sha256, release().sha256.unwrap());
}

#[test]
fn a_windows_shaped_artifact_name_needs_no_quoting() {
    // The zip fixture stands in for a Windows artifact. The derived name is a
    // single path component with no spaces of its own, so it can be joined onto
    // any directory - spaced or not - without quoting.
    let windows = Release {
        version: "8.4.2".to_owned(),
        platform: "windows-x64".to_owned(),
        archive_format: Some(lambo_core::catalog::ArchiveFormat::Zip),
        ..Default::default()
    };
    let name = sources::artifact_file_name(Family::Php, &windows).expect("a name");
    assert_eq!(name, "php-8.4.2-windows-x64.zip");
    assert!(!name.contains(' '), "the derived name has no spaces");

    // Joining it onto a spaced directory yields one path, not a command line.
    //
    // The separator is not asserted: `Path::join` is component-based, so on
    // Linux a Windows-shaped string is a single component and the join uses
    // `/`. What has to hold on every platform is that the result is one path
    // ending in the artifact name, with the space preserved and nothing quoted.
    let joined = Path::new(r"C:\Lambo Artifacts\php").join(&name);
    let rendered = joined.to_string_lossy();
    assert!(
        rendered.ends_with(&name),
        "the artifact name did not survive the join: {rendered}"
    );
    assert!(
        rendered.contains("Lambo Artifacts"),
        "the spaced directory was altered: {rendered}"
    );
    assert!(
        !rendered.contains('"') && !rendered.contains('\''),
        "a path must never need shell quoting: {rendered}"
    );
}

#[test]
fn an_override_that_pins_a_digest_still_uses_a_local_artifact() {
    let temp = TempDir::new("override-plus-local");
    let paths = temp.home();

    // The two mechanisms answer different questions, and a user doing an
    // offline install needs both: the override says what the artifact must hash
    // to, the artifacts directory says the bytes are already here. Consulting
    // the override's URL first would send them to the network for bytes they
    // already have - which was the bug this pins down.
    let name = sources::artifact_file_name(Family::Php, &release()).expect("a name");
    stage_artifact(&sources::artifacts_dir(&SourcesConfig::default(), &paths).join(&name));

    let mut release = release();
    release.from_override = true;

    let resolved = sources::resolve(
        Family::Php,
        &release,
        &SourcesConfig::default(),
        &paths,
        true,
    );
    assert_eq!(
        resolved.kind,
        SourceKind::Local,
        "a local artifact must be preferred over fetching the override's URL"
    );
    // The digest still comes from the override, so the local file is held to it.
    assert_eq!(resolved.artifact.sha256, release.sha256);

    // And it really installs offline, which is the point of the combination.
    let installed = php::install_release(
        &paths,
        &release,
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect("an offline install from a verified local artifact");
    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("manifest");
    assert_eq!(manifest.source_kind, SourceKind::Local.as_str());
}

#[test]
fn a_stale_local_artifact_fails_against_the_override_digest() {
    let temp = TempDir::new("stale-local");
    let paths = temp.home();

    // The consequence of preferring the local file: if it is not the artifact
    // the digest describes, that must surface as a mismatch rather than being
    // installed. Being on the local disk earns nothing.
    let dir = sources::artifacts_dir(&SourcesConfig::default(), &paths);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("php-8.4.2-linux-x64.tar.gz"),
        b"an older artifact left behind by a previous version",
    )
    .unwrap();

    let mut release = release();
    release.from_override = true;
    let error = php::install_release(
        &paths,
        &release,
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect_err("a stale local artifact must not be installed");
    assert!(
        matches!(error, Error::ChecksumMismatch { .. }),
        "expected a checksum mismatch, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

#[test]
fn an_install_records_where_it_came_from() {
    let temp = TempDir::new("provenance");
    let paths = temp.home();

    let installed = php::install_release(
        &paths,
        &release(),
        &LocalDownloader,
        &SourcesConfig::default(),
    )
    .expect("install");
    let manifest = lambo_core::runtime::read_manifest(&installed.path).expect("manifest");

    assert_eq!(manifest.source_kind, SourceKind::Catalogue.as_str());
    assert_eq!(manifest.sha256, release().sha256.unwrap());
    assert_eq!(manifest.platform, "linux-x64");
}

#[test]
fn a_manifest_from_an_older_lambo_reports_unknown_rather_than_guessing() {
    let temp = TempDir::new("old-manifest");
    let dir = temp.join("php/8.4.2");
    std::fs::create_dir_all(&dir).unwrap();

    // A manifest written before source tracking existed: no `source_kind` field
    // at all. It must still parse, and must not claim an origin.
    let legacy = r#"{
        "kind": "php",
        "version": "8.4.2",
        "platform": "linux-x64",
        "source_url": "https://example.com/php.tar.gz",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "installed_at": 1700000000,
        "executable": "",
        "files": []
    }"#;
    std::fs::write(dir.join(".lambo-install.json"), legacy).unwrap();

    let manifest = lambo_core::runtime::read_manifest(&dir).expect("an old manifest still parses");
    assert_eq!(manifest.source_kind, SourceKind::UNKNOWN);
    assert_eq!(
        SourceKind::parse(&manifest.source_kind),
        SourceKind::Unknown,
        "unknown provenance must not be read as the official catalogue"
    );
}

// ---------------------------------------------------------------------------
// Release validation
// ---------------------------------------------------------------------------

/// A minimal, fully-specified release: what a releasable entry looks like.
fn complete_release() -> Release {
    Release {
        version: "8.4.2".to_owned(),
        platform: "linux-x64".to_owned(),
        url: "https://example.com/php-8.4.2.tar.gz".to_owned(),
        sha256: Some("a".repeat(64)),
        archive_format: Some(lambo_core::catalog::ArchiveFormat::TarGz),
        ..Default::default()
    }
}

fn catalog_with(release: Release) -> Catalog {
    Catalog {
        php: vec![release],
        ..Catalog::default()
    }
}

#[test]
fn a_complete_entry_passes_the_release_gate() {
    let catalog = catalog_with(complete_release());
    assert!(catalog.validate().is_empty(), "development validation");
    assert!(
        catalog.release_blocking_issues().is_empty(),
        "a fully specified entry is releasable: {:?}",
        catalog.release_blocking_issues()
    );
}

#[test]
fn a_missing_digest_passes_development_but_blocks_a_release() {
    let mut release = complete_release();
    release.sha256 = None;
    release.checksum_url = None;
    let catalog = catalog_with(release);

    // This is the distinction the whole design rests on: unavailable
    // verification metadata is a fact a running Lambo reports and fails closed
    // on, but it is not something a release may ship.
    assert!(
        catalog.blocking_issues().is_empty(),
        "a null digest is legitimate at run time"
    );
    let blocking = catalog.release_blocking_issues();
    assert_eq!(blocking.len(), 1, "{blocking:?}");
    assert!(
        blocking[0].message.contains("no pinned sha256"),
        "{}",
        blocking[0]
    );
}

#[test]
fn a_malformed_digest_is_rejected_by_both() {
    let mut release = complete_release();
    release.sha256 = Some("not-a-digest".to_owned());
    let catalog = catalog_with(release);

    assert!(!catalog.blocking_issues().is_empty(), "development");
    assert!(!catalog.release_blocking_issues().is_empty(), "release");
}

#[test]
fn two_entries_claiming_the_same_slot_are_rejected() {
    let catalog = Catalog {
        php: vec![complete_release(), complete_release()],
        ..Catalog::default()
    };
    let blocking = catalog.release_blocking_issues();
    assert!(
        blocking
            .iter()
            .any(|issue| issue.message.contains("duplicate")),
        "{blocking:?}"
    );
}

#[test]
fn an_executable_that_would_escape_the_archive_is_rejected_at_the_gate() {
    let mut release = complete_release();
    release.executable = Some("../../bin/php".to_owned());
    let catalog = catalog_with(release);

    // Today this only surfaces at install time, on a user's machine, after the
    // download completes. The gate catches it before it ships.
    let blocking = catalog.release_blocking_issues();
    assert!(
        blocking
            .iter()
            .any(|issue| issue.message.contains("executable")),
        "{blocking:?}"
    );
}

#[test]
fn the_shipped_catalogue_reports_its_real_release_readiness() {
    let catalog = Catalog::embedded().expect("the embedded catalogue");

    // Development validation is clean: nothing here is broken, the entries are
    // simply not finished.
    assert!(
        catalog.blocking_issues().is_empty(),
        "{:?}",
        catalog.blocking_issues()
    );

    // Every entry currently lacks a pinned digest, so every entry blocks a
    // release. If this count ever drops below the entry count, some digests
    // have been pinned - which is the goal, and this assertion should be
    // updated rather than deleted.
    let entries = lambo_core::catalog::Family::ALL
        .iter()
        .map(|family| catalog.releases(*family).len())
        .sum::<usize>();
    let blocking = catalog.release_blocking_issues();
    assert!(
        !blocking.is_empty(),
        "the catalogue is now releasable; update this test to assert it stays that way"
    );
    assert!(
        blocking.len() <= entries,
        "{} blocking issues for {entries} entries",
        blocking.len()
    );
}
