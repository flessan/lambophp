//! The download catalogue: which archive to fetch for which platform.
//!
//! URLs and checksums are *data*, not code, for three reasons:
//!
//! - Upstream moves releases without telling anyone, and a stale URL must not
//!   require a new Lambo release to fix. A user can drop a corrected
//!   `php.json` into `$LAMBO_HOME/config/catalogs/` and carry on.
//! - Checksums belong next to the thing they protect, where they can be
//!   audited and updated.
//! - It keeps every platform's information in one reviewable place instead of
//!   scattered through `#[cfg]` branches.
//!
//! A starter catalogue ships inside the binary ([`Catalog::embedded`]) so
//! `lambo php list` works offline; the on-disk catalogue is merged on top of
//! it, with user entries winning.
//!
//! Checksums may be `null`. That does **not** mean "unchecked": the downloader
//! then looks for the upstream `.sha256` sidecar and refuses the download when
//! neither is available. See [`crate::download`].

use std::fmt;
use std::fs;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::download::Artifact;
use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::version::VersionSpec;

/// The catalogue that ships inside the binary.
pub const EMBEDDED: &str = include_str!("../catalogs/default.json");

/// A runtime family with downloadable releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    /// PHP.
    Php,
    /// Apache httpd.
    Apache,
    /// MariaDB.
    Mariadb,
    /// Oracle MySQL.
    Mysql,
    /// The database manager (Adminer).
    DbUi,
}

impl Family {
    /// Every family, in display order.
    pub const ALL: [Self; 5] = [
        Self::Php,
        Self::Apache,
        Self::Mariadb,
        Self::Mysql,
        Self::DbUi,
    ];

    /// Key used in the catalogue document.
    pub fn key(self) -> &'static str {
        match self {
            Self::Php => "php",
            Self::Apache => "apache",
            Self::Mariadb => "mariadb",
            Self::Mysql => "mysql",
            Self::DbUi => "dbui",
        }
    }

    /// File name of the per-family override in `config/catalogs/`.
    pub fn override_file_name(self) -> String {
        format!("{}.json", self.key())
    }

    /// Human-readable name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Php => "PHP",
            Self::Apache => "Apache",
            Self::Mariadb => "MariaDB",
            Self::Mysql => "MySQL",
            Self::DbUi => "database manager",
        }
    }
}

impl fmt::Display for Family {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// How an artifact is packaged.
///
/// Kept explicit rather than sniffed from the URL alone, because a URL is free
/// text and an extension Lambo cannot unpack has to be caught when the
/// catalogue is validated - not after a multi-hundred-megabyte download has
/// been fetched and verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFormat {
    /// A ZIP archive.
    Zip,
    /// A gzip-compressed tar.
    #[serde(rename = "tar.gz")]
    TarGz,
    /// An xz-compressed tar.
    #[serde(rename = "tar.xz")]
    TarXz,
    /// A single file that is copied into place, never extracted (Adminer).
    SingleFile,
}

impl ArchiveFormat {
    /// Infers the format from a file name, when the extension is recognised.
    pub fn from_file_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            Some(Self::TarXz)
        } else if lower.ends_with(".php") {
            Some(Self::SingleFile)
        } else {
            None
        }
    }

    /// The catalogue key, as written in the JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::SingleFile => "php",
        }
    }

    /// Whether [`crate::archive`] can unpack this format.
    ///
    /// `TarXz` is a real release format that Lambo does not yet unpack; it is
    /// representable so the catalogue can describe the artifact honestly and
    /// validation can reject it up front instead of after a download.
    pub fn is_extractable(self) -> bool {
        matches!(self, Self::Zip | Self::TarGz)
    }
}

impl fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What uniquely identifies one downloadable artifact.
///
/// Two entries with the same identity are a catalogue bug: resolution between
/// them would be arbitrary, and which one a user got would depend on ordering
/// in a file they did not write.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactIdentity {
    /// Runtime family.
    pub family: Family,
    /// Exact version.
    pub version: String,
    /// Platform key.
    pub platform: String,
}

impl fmt::Display for ArtifactIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ({})", self.family, self.version, self.platform)
    }
}

/// One downloadable release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// Exact version, e.g. `8.4.2`.
    pub version: String,
    /// Platform key, e.g. `windows-x64` (see [`crate::platform::Platform::key`]).
    pub platform: String,
    /// Download URL. Must be `https://` (or `file://` for a local mirror).
    pub url: String,
    /// Pinned SHA-256 digest, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// URL of an upstream checksum file; defaults to `<url>.sha256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_url: Option<String>,
    /// Packaging format. Inferred from the URL when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_format: Option<ArchiveFormat>,
    /// File name the artifact is cached under. Derived from the URL when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Expected size in bytes, when the upstream publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Release channel, e.g. `stable`, `rc`, `nightly`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Path of the primary executable inside the archive, relative to the
    /// runtime root. Lets an artifact declare its own layout instead of Lambo
    /// guessing from a list of candidate names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Whether this entry came from the user's own `config/catalogs/`.
    ///
    /// Not part of the document: it is set while merging, so an install can
    /// record whether it used an artifact the user named or one Lambo shipped.
    /// Tracked per entry rather than per family, because an override file may
    /// replace one version and leave the rest untouched.
    #[serde(skip)]
    pub from_override: bool,
}

impl Default for Release {
    /// An empty release, so callers can build one field at a time:
    /// `Release { version: …, ..Default::default() }`.
    ///
    /// Every optional field defaults to `None`, which is the honest value:
    /// absent metadata, not invented metadata.
    fn default() -> Self {
        Self {
            version: String::new(),
            platform: String::new(),
            url: String::new(),
            sha256: None,
            checksum_url: None,
            archive_format: None,
            filename: None,
            size: None,
            channel: None,
            executable: None,
            from_override: false,
        }
    }
}

impl Release {
    /// A release identified by version, platform and URL.
    ///
    /// The metadata fields stay `None` until the catalogue declares them.
    pub fn new(
        version: impl Into<String>,
        platform: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            version: version.into(),
            platform: platform.into(),
            url: url.into(),
            ..Self::default()
        }
    }

    /// The parsed version, or `None` when the catalogue is malformed.
    pub fn parsed_version(&self) -> Option<Version> {
        Version::parse(&self.version).ok()
    }

    /// The platform this release was published for, parsed.
    ///
    /// `None` means the catalogue names a platform Lambo does not know.
    pub fn platform_parsed(&self) -> Option<crate::platform::Platform> {
        crate::platform::Platform::from_key(&self.platform)
    }

    /// This release's identity, for duplicate detection.
    pub fn identity(&self, family: Family) -> ArtifactIdentity {
        ArtifactIdentity {
            family,
            version: self.version.clone(),
            platform: self.platform.clone(),
        }
    }

    /// The packaging format: the declared one, else inferred from the URL.
    ///
    /// `None` when neither is available, which validation reports rather than
    /// treating as "probably a zip".
    pub fn resolved_format(&self) -> Option<ArchiveFormat> {
        if let Some(format) = self.archive_format {
            return Some(format);
        }
        let name = self.filename.as_deref().unwrap_or(&self.url);
        ArchiveFormat::from_file_name(name.rsplit('/').next().unwrap_or(name))
    }

    /// Turns the release into a downloadable, verifiable artifact.
    pub fn artifact(&self) -> Artifact {
        Artifact {
            url: self.url.clone(),
            sha256: self.sha256.clone(),
            checksum_url: Some(
                self.checksum_url
                    .clone()
                    .unwrap_or_else(|| format!("{}.sha256", self.url)),
            ),
            file_name_override: self.filename.clone(),
            size: self.size,
        }
    }
}

/// The whole catalogue document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Catalog {
    /// Document format version, so a future Lambo can migrate old files.
    pub schema: u32,
    /// PHP releases.
    pub php: Vec<Release>,
    /// Apache releases.
    pub apache: Vec<Release>,
    /// MariaDB releases.
    pub mariadb: Vec<Release>,
    /// MySQL releases.
    pub mysql: Vec<Release>,
    /// Database manager releases.
    pub dbui: Vec<Release>,
    /// Which families were replaced by a file in `config/catalogs/`.
    ///
    /// Not part of the document: it is recorded while loading so a diagnostic
    /// can say whether a broken entry is the user's own edit or one Lambo
    /// shipped. Those two call for opposite fixes, and guessing wrong sends
    /// the user to edit a file that does not exist.
    #[serde(skip)]
    pub overridden: Vec<Family>,
}

impl Catalog {
    /// Did a file in `config/catalogs/` supply this family's releases?
    pub fn is_overridden(&self, family: Family) -> bool {
        self.overridden.contains(&family)
    }
}

/// How serious a catalogue problem is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    /// The entry cannot work: installation would fail or install the wrong thing.
    Error,
    /// The entry is suspicious but may still be correct.
    Warning,
}

impl IssueSeverity {
    /// The marker `lambo doctor` prints.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warn",
        }
    }
}

/// One problem found while validating a catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIssue {
    /// How serious it is.
    pub severity: IssueSeverity,
    /// Which family the entry belongs to.
    pub family: Family,
    /// The entry's version, as written.
    pub version: String,
    /// The entry's platform, as written.
    pub platform: String,
    /// What is wrong, in one sentence.
    pub message: String,
}

impl fmt::Display for CatalogIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} ({}): {}",
            self.family, self.version, self.platform, self.message
        )
    }
}

impl Catalog {
    /// Checks every entry for problems that would only surface mid-install.
    ///
    /// A catalogue is data someone edits by hand, and the failure modes are
    /// silent: a `.tar.xz` URL downloads and verifies perfectly and then cannot
    /// be unpacked; a typo in a platform key means the entry never matches and
    /// the user is told the version does not exist. Catching those here turns a
    /// confusing runtime failure into a line of `lambo doctor` output.
    ///
    /// Returns every issue rather than stopping at the first, because a
    /// hand-edited override file usually has more than one problem.
    pub fn validate(&self) -> Vec<CatalogIssue> {
        let mut issues = Vec::new();
        let mut seen: std::collections::HashSet<ArtifactIdentity> =
            std::collections::HashSet::new();

        for family in Family::ALL {
            for release in self.releases(family) {
                let id = release.identity(family);
                let at = |severity, message: String| CatalogIssue {
                    severity,
                    family,
                    version: release.version.clone(),
                    platform: release.platform.clone(),
                    message,
                };

                // --- identity: two entries claiming the same slot -------------
                if !seen.insert(id.clone()) {
                    issues.push(at(
                        IssueSeverity::Error,
                        "duplicate entry: two artifacts claim the same family, version and \
                         platform, so which one is installed depends on file order"
                            .to_owned(),
                    ));
                }

                // --- version ---------------------------------------------------
                if release.version.trim().is_empty() {
                    issues.push(at(IssueSeverity::Error, "version is empty".to_owned()));
                } else if release.parsed_version().is_none() {
                    issues.push(at(
                        IssueSeverity::Error,
                        format!("`{}` is not a valid semantic version", release.version),
                    ));
                }

                // --- platform --------------------------------------------------
                match crate::platform::Platform::from_key(&release.platform) {
                    None => issues.push(at(
                        IssueSeverity::Error,
                        format!(
                            "`{}` is not a known platform key; expected <os>-<arch>, e.g. \
                             windows-x64 or linux-arm64",
                            release.platform
                        ),
                    )),
                    Some(platform) if !platform.is_supported() => issues.push(at(
                        IssueSeverity::Warning,
                        format!("`{}` is not a platform Lambo manages", release.platform),
                    )),
                    Some(_) => {}
                }

                // --- URL -------------------------------------------------------
                if let Err(error) = crate::download::require_https(&release.url) {
                    issues.push(at(IssueSeverity::Error, error.to_string()));
                }

                // --- packaging -------------------------------------------------
                // The database manager ships as a single PHP file that is copied
                // into place; every other family must unpack.
                let single_file_expected = family == Family::DbUi;
                match release.resolved_format() {
                    None => issues.push(at(
                        IssueSeverity::Error,
                        "no archive format could be determined from the URL, and none is \
                         declared in `archive_format`"
                            .to_owned(),
                    )),
                    Some(ArchiveFormat::SingleFile) if !single_file_expected => issues.push(at(
                        IssueSeverity::Warning,
                        "a single file was declared for a runtime family, which is normally \
                         distributed as an archive"
                            .to_owned(),
                    )),
                    Some(format) if !format.is_extractable() && !single_file_expected => {
                        issues.push(at(
                            IssueSeverity::Error,
                            format!(
                                "the artifact is packaged as {format}, which Lambo cannot \
                                 unpack; nothing can be installed from it"
                            ),
                        ));
                    }
                    Some(_) => {}
                }

                // --- checksum shape --------------------------------------------
                // A null checksum is legitimate: verification metadata is
                // unavailable and the download fails closed. A malformed one is
                // a typo, and would fail much later in a more confusing way.
                if let Some(digest) = &release.sha256 {
                    if crate::sha256::parse_digest(digest).is_none() {
                        issues.push(at(
                            IssueSeverity::Error,
                            "sha256 is not a 64-character hex digest; use null if the digest \
                             is unknown rather than a placeholder"
                                .to_owned(),
                        ));
                    }
                }

                // --- sidecar URL -----------------------------------------------
                if let Some(url) = &release.checksum_url {
                    if let Err(error) = crate::download::require_https(url) {
                        issues.push(at(
                            IssueSeverity::Error,
                            format!("checksum_url is not usable: {error}"),
                        ));
                    }
                }
            }
        }
        issues
    }

    /// Validates the catalogue as a release gate.
    ///
    /// Stricter than [`Catalog::validate`], which is what a running Lambo needs:
    /// there, a missing digest is legitimate, because verification metadata
    /// being unavailable is a fact about the world and the download fails
    /// closed. Here, a missing digest means the release is not finished, and
    /// shipping it would publish a catalogue whose entries cannot be installed
    /// by anybody.
    ///
    /// This is the check that answers "are all production artifacts installable
    /// and verified?" without downloading anything: it validates metadata, so it
    /// is cheap enough to run on every commit.
    pub fn validate_release(&self) -> Vec<CatalogIssue> {
        let mut issues = self.validate();

        for family in Family::ALL {
            for release in self.releases(family) {
                let at = |message: String| CatalogIssue {
                    severity: IssueSeverity::Error,
                    family,
                    version: release.version.clone(),
                    platform: release.platform.clone(),
                    message,
                };

                // The one rule that differs from `validate`: a release entry
                // must carry its own digest. Relying on a `.sha256` sidecar is
                // fine at run time, but a release should not depend on a file
                // Lambo does not control staying where it is.
                if release.sha256.is_none() {
                    issues.push(at(
                        "no pinned sha256, so this entry cannot be installed from a release; \
                         calculate the digest and record it"
                            .to_owned(),
                    ));
                }

                // A declared executable has to be a relative path inside the
                // archive. An absolute or escaping path is caught at install
                // time today, which is late and confusing; catching it here
                // keeps a bad entry out of the release entirely.
                if let Some(executable) = &release.executable {
                    if !is_relative_inside_archive(executable) {
                        issues.push(at(format!(
                            "executable `{executable}` must be a relative path inside the \
                             archive, with no `..` and no leading separator"
                        )));
                    }
                }

                // Size, when declared, must be plausible: a zero-byte artifact
                // is never what was meant.
                if let Some(0) = release.size {
                    issues.push(at("size is declared as 0 bytes".to_owned()));
                }
            }
        }
        issues
    }

    /// Only the entries that block a release.
    pub fn release_blocking_issues(&self) -> Vec<CatalogIssue> {
        self.validate_release()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect()
    }

    /// Only the entries that cannot possibly install.
    pub fn blocking_issues(&self) -> Vec<CatalogIssue> {
        self.validate()
            .into_iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .collect()
    }

    /// The catalogue compiled into the binary.
    pub fn embedded() -> Result<Self> {
        serde_json::from_str(EMBEDDED).map_err(|source| Error::Json {
            path: Path::new("catalogs/default.json").to_path_buf(),
            source,
        })
    }

    /// Loads the embedded catalogue plus any user overrides.
    ///
    /// A malformed override is reported rather than ignored: silently falling
    /// back to the embedded catalogue would leave a user staring at a stale
    /// URL they just tried to fix.
    pub fn load(paths: &Paths) -> Result<Self> {
        let mut catalog = Self::embedded()?;
        let directory = paths.catalogs_dir();

        for family in Family::ALL {
            let path = directory.join(family.override_file_name());
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let parsed: Catalog =
                serde_json::from_str(&text).map_err(|source| Error::Json { path, source })?;
            catalog.merge(family, parsed.releases(family));
            catalog.overridden.push(family);
        }
        Ok(catalog)
    }

    /// The releases of one family.
    pub fn releases(&self, family: Family) -> &[Release] {
        match family {
            Family::Php => &self.php,
            Family::Apache => &self.apache,
            Family::Mariadb => &self.mariadb,
            Family::Mysql => &self.mysql,
            Family::DbUi => &self.dbui,
        }
    }

    /// The mutable release list of one family.
    fn releases_mut(&mut self, family: Family) -> &mut Vec<Release> {
        match family {
            Family::Php => &mut self.php,
            Family::Apache => &mut self.apache,
            Family::Mariadb => &mut self.mariadb,
            Family::Mysql => &mut self.mysql,
            Family::DbUi => &mut self.dbui,
        }
    }

    /// Adds or replaces releases, keyed by `(version, platform)`.
    pub fn merge(&mut self, family: Family, releases: &[Release]) {
        let target = self.releases_mut(family);
        for release in releases {
            match target.iter_mut().find(|existing| {
                existing.version == release.version && existing.platform == release.platform
            }) {
                Some(existing) => {
                    let mut merged = release.clone();
                    merged.from_override = true;
                    *existing = merged;
                }
                None => {
                    let mut added = release.clone();
                    added.from_override = true;
                    target.push(added);
                }
            }
        }
    }

    /// Releases of one family available for `platform`, newest first.
    pub fn available(&self, family: Family, platform: &str) -> Vec<Release> {
        let mut releases: Vec<Release> = self
            .releases(family)
            .iter()
            .filter(|release| release.platform == platform)
            .cloned()
            .collect();
        releases.sort_by(|a, b| match (a.parsed_version(), b.parsed_version()) {
            (Some(a), Some(b)) => b.cmp(&a),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.version.cmp(&b.version),
        });
        releases
    }

    /// The best release matching a version spec on a platform.
    ///
    /// `stable` means "newest non-prerelease", matching how
    /// [`crate::runtime::select`] reads the local inventory.
    pub fn find(&self, family: Family, spec: &VersionSpec, platform: &str) -> Option<Release> {
        let candidates = self.available(family, platform);
        candidates
            .into_iter()
            .find(|release| match (spec, release.parsed_version()) {
                (VersionSpec::Req(requirement), Some(version)) => requirement.matches(&version),
                (VersionSpec::Stable, Some(version)) => version.pre.is_empty(),
                (VersionSpec::Latest, Some(_)) => true,
                (VersionSpec::Nightly, Some(version)) => version.pre.as_str().contains("nightly"),
                (_, None) => false,
            })
    }

    /// Every distinct version of a family, newest first, across platforms.
    pub fn versions(&self, family: Family) -> Vec<String> {
        let mut versions: Vec<String> = Vec::new();
        for release in self.releases(family) {
            if !versions.contains(&release.version) {
                versions.push(release.version.clone());
            }
        }
        versions.sort_by(|a, b| match (Version::parse(a), Version::parse(b)) {
            (Ok(a), Ok(b)) => b.cmp(&a),
            _ => a.cmp(b),
        });
        versions
    }
}

/// Whether a declared executable path stays inside the archive it names.
///
/// Rejects absolute paths and any `..` component. This is metadata validation,
/// not a substitute for the extraction-time checks in [`crate::archive`]; it
/// exists so a malformed entry is caught before it ships rather than at install
/// time on a user's machine.
fn is_relative_inside_archive(value: &str) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    let normalised = value.replace('\\', "/");
    if normalised.starts_with('/') {
        return false;
    }
    !normalised
        .split('/')
        .any(|component| component == ".." || component.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn the_embedded_catalogue_is_valid() {
        let catalog = Catalog::embedded().expect("the shipped catalogue must parse");
        assert_eq!(catalog.schema, 1);
        assert!(!catalog.php.is_empty(), "PHP releases must be listed");
        assert!(!catalog.apache.is_empty(), "Apache releases must be listed");
        assert!(
            !catalog.mariadb.is_empty(),
            "MariaDB releases must be listed"
        );
        assert!(
            !catalog.dbui.is_empty(),
            "the database manager must be listed"
        );

        for release in Family::ALL
            .iter()
            .flat_map(|family| catalog.releases(*family))
        {
            assert!(
                release.url.starts_with("https://"),
                "`{}` must be an https URL",
                release.url
            );
            assert!(
                release.parsed_version().is_some(),
                "`{}` is not a valid version",
                release.version
            );
            assert!(
                !release.platform.is_empty(),
                "{} must name a platform",
                release.version
            );
        }
    }

    #[test]
    fn the_embedded_catalogue_covers_the_supported_platforms() {
        let catalog = Catalog::embedded().unwrap();
        for platform in ["windows-x64", "linux-x64", "macos-x64", "macos-arm64"] {
            assert!(
                !catalog.available(Family::Php, platform).is_empty(),
                "PHP must be installable on {platform}"
            );
        }
        assert!(
            !catalog.available(Family::Apache, "windows-x64").is_empty(),
            "Apache must be installable on Windows"
        );
        assert!(
            !catalog.available(Family::Mariadb, "windows-x64").is_empty(),
            "MariaDB must be installable on Windows"
        );
    }

    #[test]
    fn finding_a_release_honours_version_specs() {
        let catalog = Catalog::embedded().unwrap();
        let platform = "windows-x64";

        let stable = catalog
            .find(Family::Php, &VersionSpec::Stable, platform)
            .unwrap();
        assert!(stable.parsed_version().unwrap().pre.is_empty());

        let pinned: VersionSpec = "8.4".parse().unwrap();
        let found = catalog.find(Family::Php, &pinned, platform).unwrap();
        assert!(
            found.version.starts_with("8.4."),
            "{} does not match 8.4",
            found.version
        );

        let impossible: VersionSpec = "5.6".parse().unwrap();
        assert!(catalog.find(Family::Php, &impossible, platform).is_none());
        assert!(
            catalog
                .find(Family::Php, &VersionSpec::Stable, "plan9-mips")
                .is_none()
        );
    }

    #[test]
    fn releases_become_verifiable_artifacts() {
        let catalog = Catalog::embedded().unwrap();
        let release = catalog.available(Family::Php, "windows-x64").pop().unwrap();
        let artifact = release.artifact();

        assert_eq!(artifact.url, release.url);
        assert_eq!(artifact.sha256, release.sha256);
        assert!(
            artifact.checksum_url.is_some(),
            "without a pinned digest the sidecar URL must be set"
        );
        assert!(!artifact.file_name().is_empty());
    }

    #[test]
    fn user_overrides_win_over_the_embedded_catalogue() {
        let temp = TempDir::new();
        let paths = temp.home();

        let catalog = Catalog::embedded().unwrap();
        let original = catalog.available(Family::Php, "windows-x64").pop().unwrap();

        let fixed = Release {
            version: original.version.clone(),
            platform: original.platform.clone(),
            url: "https://mirror.example.com/php.zip".to_owned(),
            sha256: Some("a".repeat(64)),
            checksum_url: None,
            ..Default::default()
        };
        let document = Catalog {
            php: vec![fixed.clone()],
            ..Catalog::default()
        };
        fs::write(
            paths.catalogs_dir().join(Family::Php.override_file_name()),
            serde_json::to_string_pretty(&document).unwrap(),
        )
        .unwrap();

        let loaded = Catalog::load(&paths).unwrap();
        let merged = loaded
            .available(Family::Php, "windows-x64")
            .into_iter()
            .find(|release| release.version == original.version)
            .unwrap();
        assert_eq!(merged.url, "https://mirror.example.com/php.zip");
        assert_eq!(merged.sha256.as_deref(), Some("a".repeat(64).as_str()));

        // Entries the override does not mention survive.
        assert!(
            loaded.available(Family::Apache, "windows-x64").len() > 1
                || !loaded.releases(Family::Apache).is_empty()
        );
    }

    #[test]
    fn a_broken_override_is_reported_not_ignored() {
        let temp = TempDir::new();
        let paths = temp.home();
        fs::write(paths.catalogs_dir().join("php.json"), "{ this is not json").unwrap();

        let error = Catalog::load(&paths).unwrap_err();
        assert!(matches!(error, Error::Json { .. }), "{error:?}");
    }

    #[test]
    fn versions_are_listed_newest_first() {
        let catalog = Catalog {
            schema: 1,
            php: vec![
                Release {
                    version: "8.3.14".to_owned(),
                    platform: "windows-x64".to_owned(),
                    url: "https://example.com/a.zip".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
                Release {
                    version: "8.4.2".to_owned(),
                    platform: "windows-x64".to_owned(),
                    url: "https://example.com/b.zip".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
                Release {
                    version: "8.4.2".to_owned(),
                    platform: "linux-x64".to_owned(),
                    url: "https://example.com/c.tar.gz".to_owned(),
                    sha256: None,
                    checksum_url: None,
                    ..Default::default()
                },
            ],
            ..Catalog::default()
        };

        assert_eq!(
            catalog.versions(Family::Php),
            vec!["8.4.2".to_owned(), "8.3.14".to_owned()]
        );
        assert_eq!(catalog.available(Family::Php, "windows-x64").len(), 2);
    }
    #[test]
    fn the_shipped_catalogue_installs_cleanly() {
        let catalog = Catalog::embedded().unwrap();
        let blocking = catalog.blocking_issues();
        for issue in &blocking {
            println!("[{}] {}", issue.severity.marker(), issue);
        }
        // Every entry here is a promise that Lambo can install it. An entry
        // that cannot be unpacked is worse than no entry at all: the user
        // waits through a full download and verification before finding out.
        // MySQL 8.0.40 for linux-x64 was exactly that - shipped as .tar.xz,
        // which Lambo has no decompressor for - until it was dropped.
        assert!(
            blocking.is_empty(),
            "the catalogue Lambo ships must not contain dead entries: {blocking:?}"
        );
    }

    #[test]
    fn an_artifact_lambo_cannot_unpack_is_a_blocking_issue() {
        let catalog = Catalog {
            mysql: vec![Release {
                version: "8.0.40".to_owned(),
                platform: "linux-x64".to_owned(),
                url: "https://dev.mysql.com/get/x.tar.xz".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let issues = catalog.blocking_issues();
        assert_eq!(issues.len(), 1, "{issues:?}");
        let issue = &issues[0];
        assert_eq!(issue.family, Family::Mysql);
        assert_eq!(issue.platform, "linux-x64");
        assert!(issue.message.contains("tar.xz"), "{}", issue.message);
    }

    #[test]
    fn provenance_records_which_families_came_from_an_override() {
        let temp = TempDir::new();
        let paths = temp.home();
        let directory = paths.catalogs_dir();
        std::fs::create_dir_all(&directory).unwrap();

        // Nothing overridden yet: the embedded catalogue is all there is.
        assert!(!Catalog::load(&paths).unwrap().is_overridden(Family::Php));

        std::fs::write(
            directory.join(Family::Php.override_file_name()),
            br#"{"php":[{"version":"8.4.2","platform":"linux-x64",
                    "url":"https://mirror.example.com/php.zip"}]}"#,
        )
        .unwrap();

        let catalog = Catalog::load(&paths).unwrap();
        assert!(catalog.is_overridden(Family::Php));
        assert!(!catalog.is_overridden(Family::Apache));
    }

    #[test]
    fn the_shipped_catalogue_never_ships_an_unpinned_single_file_for_an_archive_family() {
        let catalog = Catalog::embedded().unwrap();
        let warnings = catalog
            .validate()
            .into_iter()
            .filter(|i| i.severity == IssueSeverity::Warning)
            .collect::<Vec<_>>();
        // Warnings are allowed - an unmanaged platform is a warning, not a
        // defect - but the shipped catalogue is reviewed by hand, so it should
        // not accumulate any.
        assert!(
            warnings.is_empty(),
            "the shipped catalogue picked up warnings: {warnings:?}"
        );
    }
}
