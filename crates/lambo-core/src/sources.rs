//! Where a runtime artifact comes from, and how that is decided.
//!
//! Lambo can obtain an artifact from four places. This module is the single
//! place that chooses between them, so the precedence is stated once and can be
//! tested once:
//!
//! 1. **An explicit override** - a per-artifact entry in
//!    `config/catalogs/<family>.json`, which is the same mechanism `Catalog`
//!    already merges. It may carry its own `sha256`.
//! 2. **A local artifact** - a file in the artifacts directory, found by a name
//!    derived from the artifact's identity.
//! 3. **A configured mirror** - the official artifact fetched from another host.
//! 4. **The official catalogue URL.**
//!
//! # What never changes
//!
//! The *bytes* may come from any of those places. The *expected digest* may
//! only come from trusted metadata: the pinned `sha256` in the catalogue (built
//! in or overridden), or the `.sha256` sidecar published beside the **official**
//! URL.
//!
//! A mirror therefore never supplies its own checksum, and neither does a
//! directory of local files. Letting the thing that provides the bytes also
//! vouch for them is not verification, it is a formality - so a source that
//! cannot be checked against trusted metadata fails closed with
//! [`Error::VerificationUnavailable`] instead of installing.
//!
//! That is the whole security model, and it is why there is no
//! `--skip-checksum`: the flag would not weaken a check, it would remove the
//! only reason to trust what got installed.

use std::path::{Path, PathBuf};

use crate::catalog::{ArchiveFormat, Family, Release};
use crate::config::SourcesConfig;
use crate::download::Artifact;
use crate::paths::Paths;

/// How the artifact being installed was located.
///
/// Recorded in the install manifest, so `lambo doctor` can tell a user whether
/// their PHP came from the official catalogue or from a mirror their employer
/// runs - which is the first thing to check when something behaves oddly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The URL in the catalogue Lambo ships.
    Catalogue,
    /// A per-artifact entry in the user's own `config/catalogs/`.
    Override,
    /// The official artifact, fetched from a configured mirror.
    Mirror,
    /// A file already present in the artifacts directory.
    Local,
    /// An install whose provenance was never recorded.
    ///
    /// Its own variant rather than a silent default, because "we do not know"
    /// and "the official catalogue" are different claims and only one of them
    /// would be a lie.
    Unknown,
}

impl SourceKind {
    /// Stable machine name, stored in the manifest.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Catalogue => "catalogue",
            Self::Override => "override",
            Self::Mirror => "mirror",
            Self::Local => "local",
            Self::Unknown => Self::UNKNOWN,
        }
    }

    /// The value stored when provenance cannot be determined.
    pub const UNKNOWN: &'static str = "unknown";

    /// Reads a machine name back, tolerating manifests written by older Lambo.
    ///
    /// An unrecognised or absent value is [`SourceKind::Unknown`], never a
    /// guess: claiming an install came from the official catalogue when the
    /// manifest does not say so would send a debugging user somewhere useless.
    pub fn parse(value: &str) -> Self {
        match value {
            "catalogue" => Self::Catalogue,
            "override" => Self::Override,
            "mirror" => Self::Mirror,
            "local" => Self::Local,
            _ => Self::Unknown,
        }
    }

    /// Human-readable name for diagnostics.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Catalogue => "official catalogue",
            Self::Override => "configured override",
            Self::Mirror => "configured mirror",
            Self::Local => "local artifact",
            Self::Unknown => "unknown",
        }
    }
}

/// An artifact with its provenance attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// What to fetch, and the digest it must match.
    pub artifact: Artifact,
    /// How it was located.
    pub kind: SourceKind,
    /// The location itself, as recorded in the manifest and shown by `doctor`.
    ///
    /// For a mirror this is the mirror base; for a local artifact the file
    /// path; otherwise the URL.
    pub origin: String,
}

impl Resolved {
    /// An artifact that came from the catalogue URL it names.
    ///
    /// Convenience for callers that have no mirror and no override to consider,
    /// and the value every test asserts against.
    pub fn catalogue(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            kind: SourceKind::Catalogue,
            origin: url.clone(),
            artifact: Artifact {
                url,
                sha256: None,
                checksum_url: None,
                file_name_override: None,
                size: None,
            },
        }
    }
}

/// Decides where `release` comes from.
///
/// `from_override` records whether the release itself was merged in from the
/// user's `config/catalogs/`, which `Catalog` tracks while loading.
pub fn resolve(
    family: Family,
    release: &Release,
    sources: &SourcesConfig,
    paths: &Paths,
    from_override: bool,
) -> Resolved {
    let mut artifact = release.artifact();

    // 1. A local artifact, when one is present under its identity-derived name.
    //
    // This is first, and ahead of an override, because the two answer different
    // questions. An override says *which* artifact this is and what it must
    // hash to; the artifacts directory says *where the bytes already are*. A
    // user who pins a digest and places the file locally is describing an
    // offline install, and consulting the override's URL first would send them
    // to the network for bytes they already have.
    //
    // The expected digest is untouched either way, so a stale or wrong file
    // here fails verification against the override's digest rather than being
    // installed.
    if let Some(local) = local_artifact(sources, paths, family, release) {
        let origin = local.display().to_string();
        artifact.url = format!("file://{origin}");
        // The sidecar stays pointed at the official URL. A `.sha256` sitting
        // next to the local file would be vouching for itself.
        return Resolved {
            origin,
            kind: SourceKind::Local,
            artifact,
        };
    }

    // 2. An explicit override URL: the user named where to fetch this from.
    if from_override {
        return Resolved {
            origin: artifact.url.clone(),
            kind: SourceKind::Override,
            artifact,
        };
    }

    // 3. A configured mirror: same artifact, different host, same expected
    //    digest - `checksum_url` still points at the official sidecar.
    let mirror = sources.mirror.trim();
    if !mirror.is_empty() {
        let origin = mirror.to_owned();
        artifact.url = mirror_url(mirror, &artifact);
        return Resolved {
            origin,
            kind: SourceKind::Mirror,
            artifact,
        };
    }

    // 4. The official catalogue.
    Resolved {
        origin: artifact.url.clone(),
        kind: SourceKind::Catalogue,
        artifact,
    }
}

/// Rewrites an artifact onto a mirror base.
///
/// The mirror is addressed by artifact name rather than by mirroring the
/// upstream path structure, so a mirror is a flat directory of files - which is
/// what an internal artifact store or a directory share actually is.
pub fn mirror_url(base: &str, artifact: &Artifact) -> String {
    format!("{}/{}", base.trim_end_matches('/'), artifact.file_name())
}

/// The directory pre-downloaded artifacts are looked for in.
pub fn artifacts_dir(sources: &SourcesConfig, paths: &Paths) -> PathBuf {
    let configured = sources.artifacts.trim();
    if configured.is_empty() {
        paths.cache_dir().join("artifacts")
    } else {
        PathBuf::from(configured)
    }
}

/// Finds a local artifact by its identity-derived name, if one is present.
fn local_artifact(
    sources: &SourcesConfig,
    paths: &Paths,
    family: Family,
    release: &Release,
) -> Option<PathBuf> {
    let name = artifact_file_name(family, release)?;
    let candidate = artifacts_dir(sources, paths).join(name);
    candidate.is_file().then_some(candidate)
}

/// The file name a local artifact must have to be recognised.
///
/// Derived from identity - family, version, platform - never by parsing
/// whatever the file happens to be called. A name Lambo chose cannot be
/// ambiguous, and a file dropped in with any other name is simply not found
/// rather than guessed at.
///
/// `None` when the name would not be a single safe path component, so a
/// version or platform containing a separator cannot escape the directory.
pub fn artifact_file_name(family: Family, release: &Release) -> Option<String> {
    let extension = match release.resolved_format()? {
        ArchiveFormat::Zip => ".zip",
        ArchiveFormat::TarGz => ".tar.gz",
        ArchiveFormat::TarXz => ".tar.xz",
        // A single-file artifact (Adminer) has no format-implied extension.
        ArchiveFormat::SingleFile => "",
    };
    let name = format!(
        "{}-{}-{}{}",
        family.key(),
        release.version,
        release.platform,
        extension
    );
    is_safe_component(&name).then_some(name)
}

/// Whether `value` is a single path component that cannot escape a directory.
///
/// Version and platform come from catalogue data, which a user can override, so
/// this is the boundary that keeps `../../etc/passwd` from becoming a path
/// Lambo reads or writes.
fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

/// Reads a `file://` URL back into a path.
///
/// The inverse of what [`resolve`] builds, used by the downloader and by tests
/// that need to point at a real file.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let stripped = url.strip_prefix("file://")?;
    // On Windows the URL form is `file:///C:/…`; elsewhere `file:///home/…`.
    let path = if cfg!(windows) {
        stripped.trim_start_matches('/')
    } else {
        stripped
    };
    (!path.is_empty()).then(|| Path::new(path).to_path_buf())
}
