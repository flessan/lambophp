//! End-to-end install against a *real* runtime archive, when one is present.
//!
//! Everything else in the suite runs against small fixture archives describing a
//! PHP that does not exist. That proves Lambo's behaviour - resolution,
//! verification, extraction, transactional install - but not that a real
//! upstream archive installs correctly.
//!
//! This file closes the gap without committing large binaries. It reads
//! manifests from `tests/fixtures/real-artifacts/`; with none present it passes
//! immediately and says so, so normal CI never depends on this directory. See
//! the README there for how to supply an artifact.
//!
//! The production catalogue is never modified by anything here.

use std::path::{Path, PathBuf};

use lambo_core::catalog::{ArchiveFormat, Family, Release};
use lambo_core::config::SourcesConfig;
use lambo_core::download::LocalDownloader;
use lambo_core::paths::Paths;
use lambo_core::runtime::RuntimeKind;

/// Where a developer places a real artifact and its manifest.
fn real_artifacts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/real-artifacts")
}

/// A manifest describing one real artifact to test.
#[derive(Debug, serde::Deserialize)]
struct Manifest {
    family: String,
    version: String,
    platform: String,
    file: String,
    sha256: String,
    #[serde(default)]
    archive_format: Option<String>,
    #[serde(default)]
    executable: Option<String>,
}

impl Manifest {
    fn family(&self) -> Family {
        match self.family.as_str() {
            "php" => Family::Php,
            "apache" => Family::Apache,
            "mariadb" => Family::Mariadb,
            "mysql" => Family::Mysql,
            "dbui" => Family::DbUi,
            other => panic!("unknown family `{other}` in the manifest"),
        }
    }

    fn runtime_kind(&self) -> RuntimeKind {
        match self.family.as_str() {
            "php" => RuntimeKind::Php,
            "apache" => RuntimeKind::Apache,
            "mariadb" => RuntimeKind::Mariadb,
            "mysql" => RuntimeKind::Mysql,
            other => panic!("family `{other}` has no runtime directory"),
        }
    }

    fn format(&self) -> Option<ArchiveFormat> {
        self.archive_format.as_deref().map(|name| match name {
            "zip" => ArchiveFormat::Zip,
            "tar.gz" => ArchiveFormat::TarGz,
            "tar.xz" => ArchiveFormat::TarXz,
            "single-file" => ArchiveFormat::SingleFile,
            other => panic!("unknown archive_format `{other}` in the manifest"),
        })
    }
}

/// The database engine a manifest describes.
fn database_kind(manifest: &Manifest) -> lambo_core::config::DatabaseKind {
    match manifest.family.as_str() {
        "mysql" => lambo_core::config::DatabaseKind::Mysql,
        _ => lambo_core::config::DatabaseKind::Mariadb,
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = format!(
            "lambo-real-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Every manifest a developer has supplied, excluding the example.
fn manifests() -> Vec<(PathBuf, Manifest)> {
    let dir = real_artifacts();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("manifest.example.json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let manifest: Manifest = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{} is not a valid manifest: {error}", path.display()));
        found.push((path, manifest));
    }
    // Deterministic order, so a failure is reproducible.
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

#[test]
fn real_artifacts_install_and_verify() {
    let found = manifests();
    if found.is_empty() {
        // Not a skip in the harness sense, but the honest outcome: there is
        // nothing to test. CI reaches this branch and moves on.
        eprintln!(
            "no real artifacts supplied; see {}",
            real_artifacts().join("README.md").display()
        );
        return;
    }

    for (path, manifest) in &found {
        let archive = real_artifacts().join(&manifest.file);
        assert!(
            archive.is_file(),
            "{} names `{}` but it is not in {}",
            path.display(),
            manifest.file,
            real_artifacts().display()
        );

        let temp = TempDir::new();
        let paths = temp.home();

        // A catalogue holding only this entry, pointing at the local file. The
        // production catalogue is untouched.
        let release = Release {
            version: manifest.version.clone(),
            platform: manifest.platform.clone(),
            url: format!("file://{}", archive.display()),
            sha256: Some(manifest.sha256.clone()),
            checksum_url: None,
            archive_format: manifest.format(),
            filename: Some(manifest.file.clone()),
            executable: manifest.executable.clone(),
            ..Default::default()
        };

        // The digest recorded in the manifest must match the bytes, checked
        // here rather than trusting the install to notice.
        let actual = lambo_core::sha256::sha256_file(&archive)
            .unwrap_or_else(|error| panic!("cannot digest {}: {error}", archive.display()));
        assert_eq!(
            actual,
            manifest.sha256,
            "{}: the manifest pins {} but the file hashes to {actual}",
            path.display(),
            manifest.sha256
        );

        let installed = match manifest.family() {
            Family::Php => lambo_core::php::install_release(
                &paths,
                &release,
                &LocalDownloader,
                &SourcesConfig::default(),
            )
            .map(|runtime| runtime.path),
            Family::Apache => lambo_core::apache::install_release(
                &paths,
                &release,
                &LocalDownloader,
                lambo_core::platform::Os::host(),
                &SourcesConfig::default(),
            )
            .map(|apache| apache.runtime.expect("a managed runtime").path),
            Family::Mariadb | Family::Mysql => lambo_core::database::install_release(
                &paths,
                database_kind(manifest),
                &release,
                &LocalDownloader,
                lambo_core::platform::Os::host(),
                &SourcesConfig::default(),
            )
            .map(|database| database.runtime.path),
            Family::DbUi => lambo_core::dbui::install_release(
                &paths,
                &release,
                &LocalDownloader,
                &SourcesConfig::default(),
            )
            .map(|ui| ui.entry.clone()),
        }
        .unwrap_or_else(|error| {
            panic!(
                "{}: installing the real {} {} artifact failed: {error}",
                path.display(),
                manifest.family,
                manifest.version
            )
        });

        // The declared executable must actually be in the result. This is the
        // check that catches a catalogue entry describing a different archive
        // than the one it points at - the most likely real-world mistake.
        let dir = if installed.is_dir() {
            installed.clone()
        } else {
            installed.parent().expect("a parent").to_path_buf()
        };
        let integrity = lambo_core::runtime::verify(&dir, manifest.runtime_kind());
        // `describe` borrows and `is_usable` consumes, so the message is built
        // before the check rather than inside the assertion.
        let described = integrity.describe();
        assert!(
            integrity.is_usable(),
            "{}: the installed runtime is not usable: {described}",
            path.display()
        );

        // And the manifest records what was verified.
        let written = lambo_core::runtime::read_manifest(&dir)
            .unwrap_or_else(|| panic!("{}: no install manifest was written", path.display()));
        assert_eq!(written.sha256, manifest.sha256);
        assert_eq!(written.version, manifest.version);

        // A real PHP build is the only thing that can confirm the health check
        // reads real output correctly. The fixture suite proves Lambo's
        // plumbing against a stand-in; this proves the stand-in was not lying
        // about the format.
        //
        // Note that `install_release` already started this binary once: the
        // post-install gate refuses to install a runtime that will not run, so
        // reaching this line means real PHP executed. What follows checks what
        // it reported.
        if manifest.family == "php" {
            let os = lambo_core::platform::Os::host();
            let installed = lambo_core::php::list(&paths)
                .unwrap_or_else(|error| panic!("{}: cannot list runtimes: {error}", path.display()))
                .into_iter()
                .find(|runtime| runtime.name == manifest.version)
                .unwrap_or_else(|| {
                    panic!(
                        "{}: PHP {} was installed but is not listed",
                        path.display(),
                        manifest.version
                    )
                });

            let health = lambo_core::php::RuntimeHealth::check(&paths, &installed, os);
            assert!(
                health.ran,
                "{}: real PHP {} did not run: {:?}",
                path.display(),
                manifest.version,
                health.problem
            );

            // The version the binary reports must be the version the catalogue
            // entry claims. A mismatch here means the pinned entry describes a
            // different build than the archive it points at.
            let reported = health.reported_version.as_ref().map(ToString::to_string);
            assert_eq!(
                reported.as_deref(),
                Some(manifest.version.as_str()),
                "{}: PHP reported {:?} but the manifest declares {}",
                path.display(),
                reported,
                manifest.version
            );

            // The generated php.ini must be the one in effect. This is the
            // "users never edit php.ini" promise checked against a real
            // interpreter rather than against a stand-in.
            let expected_ini = lambo_core::php::php_ini_path(&installed, os);
            assert_eq!(
                health.loaded_ini.as_deref(),
                Some(expected_ini.as_path()),
                "{}: real PHP loaded {:?}, not the ini Lambo generated at {}",
                path.display(),
                health.loaded_ini,
                expected_ini.display()
            );

            // A real build always has these compiled in. An empty list here
            // would mean the module parser stopped matching real output.
            for expected in ["Core", "date", "standard"] {
                assert!(
                    health.modules.iter().any(|module| module == expected),
                    "{}: real PHP did not report `{expected}`; got {:?}",
                    path.display(),
                    health.modules
                );
            }

            eprintln!(
                "real PHP {} runs: {} extensions, configuration {}",
                manifest.version,
                health.modules.len(),
                expected_ini.display()
            );
        }

        eprintln!(
            "verified {} {} ({}) from {}",
            manifest.family, manifest.version, manifest.platform, manifest.file
        );
    }
}
