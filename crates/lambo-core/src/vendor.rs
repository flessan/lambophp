//! Resolving the downloads that have no fixed URL.
//!
//! Almost everything in the catalogue points at a versioned archive, but two
//! components cannot:
//!
//! * **Apache** is published by Apache Lounge, which keeps no persistent URL.
//!   The binaries directory itself 404s, and every filename embeds both the
//!   version and the build date, so a hard-coded URL rots as soon as a new
//!   build is cut - which is exactly what broke Apache installs before: the
//!   index page answered `200` with an error page, that page was cached under
//!   a `.zip` name, and every later run failed at extraction.
//! * **Zig** publishes its released versions as keys of a JSON index rather
//!   than as links, so the answer has to be read out of a document too.
//!
//! Both resolvers therefore fetch an index and pick from it. The parsing is
//! pure and unit-tested against the real page shapes, and the selection rules
//! are the original ones: newest version, then newest build, then newest
//! toolset, so a build published only for a newer Visual Studio still wins.
//!
//! Ported from the original implementation's `vendor.go`.
//!
//! # Differences that are deliberate
//!
//! * **No regular expressions.** The previous implementation matched with one
//!   compiled pattern; the same grammar is parsed here by hand. The match is
//!   identical - the tests use the original page sample verbatim - and it keeps
//!   a regex engine out of a binary that only ever needs this one pattern.
//! * **The document is fetched through the shared transport**, so it inherits
//!   the HTTPS-only rule and the platform's certificate store instead of a
//!   private HTTP client.
//! * **Version ordering is total.** Go sorted Zig's index with a comparison
//!   that reports "not greater" in both directions for equal versions, over the
//!   keys of a hash map, so which of two equal version strings came first was
//!   random. Here equal versions keep a deterministic order, preferring the
//!   plain release over a development build of the same version.

use std::cmp::Ordering;

use serde_json::Value;

use crate::download::{self, Downloader};
use crate::error::{Error, Result};
use crate::logs::LogFn;

/// The Apache Lounge download index.
const APACHE_INDEX_URL: &str = "https://www.apachelounge.com/download/";

/// Zig's release index.
const ZIG_INDEX_URL: &str = "https://ziglang.org/download/index.json";

/// How much of the Apache index is read - the page is a few hundred kilobytes,
/// and this is the bound the previous implementation used.
const APACHE_INDEX_LIMIT: u64 = 1 << 20;

/// How much of Zig's index is read.
const ZIG_INDEX_LIMIT: u64 = 1 << 20;

/// The directory an Apache Lounge archive wraps its contents in.
const APACHE_STRIP_TOP: &str = "Apache24/";

/// What a resolver found: everything the installer needs to fetch and unpack
/// one component with nothing left to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Where to download it from.
    pub url: String,
    /// What to call the downloaded file in the cache.
    pub file_name: String,
    /// The prefix of the archive's entries to strip, with its trailing slash.
    pub strip_top: String,
    /// The version to record and show, already formatted for display.
    pub version: String,
}

/// One `httpd-*-Win64-VS*.zip` entry on an Apache Lounge index page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApacheBuild {
    /// The link exactly as the page writes it, e.g.
    /// `/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip`.
    pub link: String,
    /// The release, e.g. `2.4.68`.
    pub version: String,
    /// The build date, e.g. `260827`.
    pub build: String,
    /// The toolset tag, e.g. `18`.
    pub vs: String,
}

/// Resolves the newest Apache Win64 build listed on Apache Lounge.
///
/// The version is recorded as `2.4.68 (VS18, win64)`, which is what the
/// dashboard and `config.json` have always shown, and the archive is unpacked
/// with the `Apache24/` wrapper removed.
pub fn resolve_apache_latest(downloader: &dyn Downloader, log: &LogFn) -> Result<Resolved> {
    log("  resolving latest Apache Win64 build from apachelounge.com/download/ ...");

    let page = download::fetch_document(downloader, APACHE_INDEX_URL, APACHE_INDEX_LIMIT)?;
    let Some(best) = pick_newest_apache_build(&page) else {
        return Err(Error::Download {
            url: APACHE_INDEX_URL.to_owned(),
            reason: "no Win64 VS build listed on download page".to_owned(),
        });
    };

    log(&format!(
        "  latest Apache: {} (VS{}, build {})",
        best.version, best.vs, best.build
    ));

    Ok(Resolved {
        url: format!("https://www.apachelounge.com{}", best.link),
        file_name: format!(
            "httpd-{}-{}-Win64-VS{}.zip",
            best.version, best.build, best.vs
        ),
        strip_top: APACHE_STRIP_TOP.to_owned(),
        version: format!("{} (VS{}, win64)", best.version, best.vs),
    })
}

/// Picks the newest build on an index page.
///
/// Newest version first, then the newest build of that version, then the
/// newest toolset - so a build published only for a newer Visual Studio still
/// wins, which is what the catalogue's own default already assumed.
///
/// A 32-bit build, a different module (`mod_fcgid`) and the detached `.asc`
/// signature that sits next to every archive all fail to match, which is what
/// keeps the signature from being downloaded as if it were the archive.
pub fn pick_newest_apache_build(html: &str) -> Option<ApacheBuild> {
    let mut best: Option<(i64, i64, i64, ApacheBuild)> = None;
    let mut cursor = 0;

    while let Some(offset) = html[cursor..].find("/download/VS") {
        let start = cursor + offset;
        match match_apache_link(html, start) {
            Some((end, candidate)) => {
                let score = (
                    version_num(&candidate.version),
                    version_num(&candidate.build),
                    version_num(&candidate.vs),
                );
                let better = match &best {
                    None => true,
                    Some((version, build, vs, _)) => score > (*version, *build, *vs),
                };
                if better {
                    best = Some((score.0, score.1, score.2, candidate));
                }
                cursor = end;
            }
            // The anchor matched but the rest of the shape did not. Keep
            // looking from the next byte, the way a scanning matcher would.
            None => cursor = start + 1,
        }
    }

    best.map(|(_, _, _, build)| build)
}

/// Matches one archive link at `start`, returning where it ends and what it
/// held.
///
/// The grammar is `/download/VS(\d+)/binaries/httpd-(\d+\.\d+\.\d+)-(\d+)-Win64-VS\d+\.zip`.
fn match_apache_link(html: &str, start: usize) -> Option<(usize, ApacheBuild)> {
    let rest = html.get(start..)?.strip_prefix("/download/VS")?;
    let (vs, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix("/binaries/httpd-")?;
    let (major, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix('.')?;
    let (minor, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix('.')?;
    let (patch, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix('-')?;
    let (build, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix("-Win64-VS")?;
    let (_, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix(".zip")?;

    let end = html.len() - rest.len();
    Some((
        end,
        ApacheBuild {
            link: html[start..end].to_owned(),
            version: format!("{major}.{minor}.{patch}"),
            build: build.to_owned(),
            vs: vs.to_owned(),
        },
    ))
}

/// Takes the leading run of ASCII digits, returning it and the remainder.
///
/// A run of length zero is not a match: the grammar requires at least one
/// digit in every numeric position.
fn take_digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    Some(text.split_at(end))
}

/// Turns a dotted version, a build date or a toolset tag into a sortable
/// number: `2.4.68` becomes `2004068` and `260827` stays `260827`.
///
/// One helper covers all three fields compared by
/// [`pick_newest_apache_build`], because a value with no dots falls out as a
/// plain integer. A component that does not start with a digit counts as zero
/// and any trailing text after the digits is ignored, which is how the
/// original parsed these fields.
pub fn version_num(text: &str) -> i64 {
    let mut value = 0i64;
    for part in text.split('.') {
        value = value.saturating_mul(1000).saturating_add(leading_int(part));
    }
    value
}

/// The integer a component starts with, or zero.
fn leading_int(text: &str) -> i64 {
    let trimmed = text.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return 0;
    }
    digits[..end].parse::<i64>().map_or(0, |value| sign * value)
}

/// Resolves the newest stable Zig release.
///
/// The `master` key is not a release and is skipped. The archive is unpacked
/// with the directory it wraps its contents in removed (`zig-x86_64-windows-0.14.0/`),
/// which is derived from the file name exactly as before.
pub fn resolve_zig_latest(downloader: &dyn Downloader, log: &LogFn) -> Result<Resolved> {
    log("  resolving latest Zig stable from ziglang.org/download/index.json ...");

    let body = download::fetch_document(downloader, ZIG_INDEX_URL, ZIG_INDEX_LIMIT)?;
    let index: serde_json::Map<String, Value> =
        serde_json::from_str(&body).map_err(|error| Error::Download {
            url: ZIG_INDEX_URL.to_owned(),
            reason: error.to_string(),
        })?;

    let mut versions: Vec<&String> = index
        .keys()
        .filter(|key| key.as_str() != "master")
        .collect();
    versions.sort_by(|left, right| zig_version_order(right, left));

    let Some(latest) = versions.first() else {
        return Err(Error::Download {
            url: ZIG_INDEX_URL.to_owned(),
            reason: "no stable versions found".to_owned(),
        });
    };

    let entry = index
        .get(*latest)
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Download {
            url: ZIG_INDEX_URL.to_owned(),
            reason: format!("malformed release entry for {latest}"),
        })?;
    let asset = entry.get("x86_64-windows").ok_or_else(|| Error::Download {
        url: ZIG_INDEX_URL.to_owned(),
        reason: format!("no x86_64-windows asset for {latest}"),
    })?;
    // A release whose asset carries no `tarball` is *not* an error here: the
    // original read the field into a struct and carried on with an empty
    // string, leaving the download to fail on its own terms. A `tarball` that
    // is present but not a string is a different matter - that is a malformed
    // document, and the original refused it while decoding.
    let fields = asset.as_object().ok_or_else(|| Error::Download {
        url: ZIG_INDEX_URL.to_owned(),
        reason: format!("malformed x86_64-windows asset for {latest}"),
    })?;
    let tarball = match fields.get("tarball") {
        None => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => {
            return Err(Error::Download {
                url: ZIG_INDEX_URL.to_owned(),
                reason: format!("malformed tarball for {latest}"),
            });
        }
    };

    let file_name = tarball.rsplit('/').next().unwrap_or(tarball).to_owned();
    let strip_top = format!("{}/", file_name.strip_suffix(".zip").unwrap_or(&file_name));

    log(&format!("  latest Zig stable: {latest}"));

    Ok(Resolved {
        url: tarball.to_owned(),
        file_name,
        strip_top,
        version: (*latest).clone(),
    })
}

/// Orders two Zig version keys by their first three components.
fn zig_version_order(left: &str, right: &str) -> Ordering {
    let left_parts: Vec<&str> = left.split('.').collect();
    let right_parts: Vec<&str> = right.split('.').collect();
    for index in 0..3 {
        let a = left_parts.get(index).map_or(0, |part| leading_int(part));
        let b = right_parts.get(index).map_or(0, |part| leading_int(part));
        if a != b {
            return a.cmp(&b);
        }
    }
    Ordering::Equal
}

/// Whether `left` is a newer Zig version than `right`.
pub fn zig_version_gt(left: &str, right: &str) -> bool {
    zig_version_order(left, right) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::error::Result;

    /// The Apache Lounge index as the original tests recorded it, including
    /// every decoy: the detached signature, the 32-bit build and the
    /// `mod_fcgid` archive.
    const APACHE_PAGE_SAMPLE: &str = r#"
<a href="/download/VS17/">VS17</a>
<a href="/download/VS18/binaries/httpd-2.4.67-260504-Win64-VS18.zip">old</a>
<a href="/download/VS18/binaries/httpd-2.4.67-260504-Win64-VS18.zip.asc">sig</a>
<a href="/download/VS18/binaries/httpd-2.4.68-260617-Win64-VS18.zip">mid</a>
<a href="/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip">new</a>
<a href="/download/VS18/binaries/httpd-2.4.68-260827-win32-vs18.zip">32-bit, not win64</a>
<a href="/download/VS18/modules/mod_fcgid-2.3.10-win64-VS18.zip">not httpd</a>
"#;

    /// Zig's index, trimmed to the shape that decides the answer.
    const ZIG_INDEX: &str = r#"{
  "master": {
    "version": "0.16.0-dev.1+abc",
    "x86_64-windows": { "tarball": "https://ziglang.org/builds/zig-x86_64-windows-dev.zip" }
  },
  "0.14.1": {
    "version": "0.14.1",
    "x86_64-windows": { "tarball": "https://ziglang.org/download/0.14.1/zig-x86_64-windows-0.14.1.zip" }
  },
  "0.13.0": {
    "version": "0.13.0",
    "x86_64-windows": { "tarball": "https://ziglang.org/download/0.13.0/zig-x86_64-windows-0.13.0.zip" }
  },
  "0.14.0": {
    "version": "0.14.0",
    "x86_64-windows": { "tarball": "https://ziglang.org/download/0.14.0/zig-x86_64-windows-0.14.0.zip" }
  }
}"#;

    /// A transport that answers every request with the same document.
    struct DocumentDownloader {
        body: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Downloader for DocumentDownloader {
        fn fetch(&self, url: &str, destination: &Path) -> Result<()> {
            self.seen.lock().expect("url lock").push(url.to_owned());
            fs::write(destination, &self.body).map_err(|error| Error::io(destination, error))
        }
    }

    fn recorder() -> (LogFn, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let log: LogFn = Arc::new(move |line: &str| {
            sink.lock().expect("log lock").push(line.to_owned());
        });
        (log, lines)
    }

    fn downloader(body: &str) -> DocumentDownloader {
        DocumentDownloader {
            body: body.to_owned(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn picks_newest_apache_build() {
        let build = pick_newest_apache_build(APACHE_PAGE_SAMPLE).expect("a build must be found");
        assert_eq!(
            build.link,
            "/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip"
        );
        assert_eq!(build.version, "2.4.68");
        assert_eq!(build.build, "260827");
        assert_eq!(build.vs, "18");
    }

    #[test]
    fn picks_newest_apache_build_empty() {
        assert_eq!(pick_newest_apache_build("<html>nothing here</html>"), None);
        assert_eq!(pick_newest_apache_build(""), None);
    }

    #[test]
    fn a_newer_toolset_wins_an_otherwise_equal_build() {
        let page = "<a href=\"/download/VS17/binaries/httpd-2.4.68-260827-Win64-VS17.zip\">a</a>\n\
                    <a href=\"/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip\">b</a>";
        let build = pick_newest_apache_build(page).expect("a build must be found");
        assert_eq!(build.vs, "18");
    }

    #[test]
    fn a_newer_build_of_the_same_version_wins() {
        let page = "<a href=\"/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip\">new</a>\n\
                    <a href=\"/download/VS18/binaries/httpd-2.4.68-260504-Win64-VS18.zip\">old</a>";
        let build = pick_newest_apache_build(page).expect("a build must be found");
        assert_eq!(build.build, "260827");
    }

    #[test]
    fn a_newer_version_wins_over_a_newer_build_date() {
        // A build date is not a date in isolation: the version decides first,
        // which is why 2.4.68 must not lose to 2.4.67's later date.
        let page = "<a href=\"/download/VS18/binaries/httpd-2.4.67-269999-Win64-VS18.zip\">older</a>\n\
                    <a href=\"/download/VS18/binaries/httpd-2.4.68-260617-Win64-VS18.zip\">newer</a>";
        let build = pick_newest_apache_build(page).expect("a build must be found");
        assert_eq!(build.version, "2.4.68");
    }

    #[test]
    fn only_win64_httpd_archives_match() {
        for decoy in [
            "<a href=\"/download/VS18/binaries/httpd-2.4.68-260827-win32-vs18.zip\">32-bit</a>",
            "<a href=\"/download/VS18/modules/mod_fcgid-2.3.10-win64-VS18.zip\">module</a>",
            "<a href=\"/download/VS18/binaries/httpd-2.4-Win64-VS18.zip\">no build date</a>",
            "<a href=\"/download/VS18/binaries/httpd-2.4.68-Win64-VS18.zip\">no build number</a>",
            "<a href=\"/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS.zip\">no toolset</a>",
            "<a href=\"/download/VS/binaries/httpd-2.4.68-260827-Win64-VS18.zip\">no toolset number</a>",
            "<a href=\"/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.7z\">not a zip</a>",
        ] {
            assert_eq!(
                pick_newest_apache_build(decoy),
                None,
                "`{decoy}` must not match"
            );
        }
    }

    #[test]
    fn the_signature_line_does_not_truncate_the_archive_link() {
        // The `.asc` link ends in the archive's name; a matcher that allowed
        // trailing text would return the signature as the download.
        let build = pick_newest_apache_build(
            "<a href=\"/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip.asc\">s</a>",
        )
        .expect("the archive, not the signature");
        assert_eq!(
            build.link,
            "/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip"
        );
    }

    #[test]
    fn version_num_reads_dotted_versions_dates_and_toolsets() {
        assert_eq!(version_num("2.4.68"), 2_004_068);
        assert_eq!(version_num("2.4.9"), 2_004_009);
        assert_eq!(version_num("260827"), 260_827);
        assert_eq!(version_num("18"), 18);
        // A component that does not start with a digit counts as zero, and
        // trailing text after the digits is ignored. `4beta` starts with a
        // digit, so it is 4 and packs to 2004 - Go's `Sscanf` read the digits
        // it could and stopped; only a component with no leading digit at all
        // is zero.
        assert_eq!(version_num("2.4.rc1"), 2_004_000);
        assert_eq!(version_num("2.4beta"), 2_004);
        assert_eq!(version_num(""), 0);
        assert_eq!(version_num("x"), 0);
        assert!(version_num("2.4.68") > version_num("2.4.67"));
        // Components are packed three digits at a time, so the comparison is
        // only correct between versions with the same number of components -
        // which is all Apache publishes. `2.5` packs to 2005, below `2.4.99`'s
        // 2004099. That quirk is the original's, and is preserved.
        assert!(version_num("2.5") < version_num("2.4.99"));
    }

    #[test]
    fn resolve_apache_latest_reports_the_catalogue_shape() {
        let (log, lines) = recorder();
        let downloader = downloader(APACHE_PAGE_SAMPLE);

        let resolved = resolve_apache_latest(&downloader, &log).expect("resolution must succeed");

        assert_eq!(
            resolved.url,
            "https://www.apachelounge.com/download/VS18/binaries/httpd-2.4.68-260827-Win64-VS18.zip"
        );
        assert_eq!(resolved.file_name, "httpd-2.4.68-260827-Win64-VS18.zip");
        assert_eq!(resolved.strip_top, "Apache24/");
        assert_eq!(resolved.version, "2.4.68 (VS18, win64)");

        let lines = lines.lock().expect("log lock").clone();
        assert_eq!(
            lines,
            vec![
                "  resolving latest Apache Win64 build from apachelounge.com/download/ ..."
                    .to_owned(),
                "  latest Apache: 2.4.68 (VS18, build 260827)".to_owned(),
            ]
        );

        // The index was fetched over the shared transport, over HTTPS.
        assert_eq!(
            downloader.seen.lock().expect("url lock").clone(),
            vec![APACHE_INDEX_URL.to_owned()]
        );
    }

    #[test]
    fn resolve_apache_latest_fails_when_no_build_is_listed() {
        let (log, _lines) = recorder();
        let downloader = downloader("<html><body>maintenance</body></html>");

        let error = resolve_apache_latest(&downloader, &log).expect_err("no build is an error");
        assert!(error.to_string().contains("no Win64 VS build listed"));
    }

    #[test]
    fn resolve_zig_latest_picks_the_newest_stable() {
        let (log, lines) = recorder();
        let downloader = downloader(ZIG_INDEX);

        let resolved = resolve_zig_latest(&downloader, &log).expect("resolution must succeed");

        assert_eq!(resolved.version, "0.14.1");
        assert_eq!(
            resolved.url,
            "https://ziglang.org/download/0.14.1/zig-x86_64-windows-0.14.1.zip"
        );
        assert_eq!(resolved.file_name, "zig-x86_64-windows-0.14.1.zip");
        assert_eq!(resolved.strip_top, "zig-x86_64-windows-0.14.1/");

        let lines = lines.lock().expect("log lock").clone();
        assert_eq!(
            lines,
            vec![
                "  resolving latest Zig stable from ziglang.org/download/index.json ...".to_owned(),
                "  latest Zig stable: 0.14.1".to_owned(),
            ]
        );
    }

    #[test]
    fn resolve_zig_latest_rejects_an_index_without_releases() {
        let (log, _lines) = recorder();
        let downloader = downloader(r#"{"master":{"x86_64-windows":{"tarball":"x.zip"}}}"#);

        let error = resolve_zig_latest(&downloader, &log).expect_err("master is not a release");
        assert!(error.to_string().contains("no stable versions found"));
    }

    #[test]
    fn resolve_zig_latest_rejects_a_release_without_a_windows_asset() {
        let (log, _lines) = recorder();
        let downloader = downloader(r#"{"0.14.1":{"x86_64-linux":{"tarball":"x.tar.xz"}}}"#);

        let error = resolve_zig_latest(&downloader, &log).expect_err("no Windows asset");
        assert!(
            error
                .to_string()
                .contains("no x86_64-windows asset for 0.14.1")
        );
    }

    #[test]
    fn resolve_zig_latest_rejects_a_document_that_is_not_an_object() {
        let (log, _lines) = recorder();
        let downloader = downloader("[1, 2, 3]");

        let error = resolve_zig_latest(&downloader, &log).expect_err("an array is not an index");
        assert!(matches!(error, Error::Download { .. }));
    }

    #[test]
    fn zig_versions_compare_on_three_components() {
        assert!(zig_version_gt("0.14.1", "0.13.0"));
        assert!(zig_version_gt("0.15.0", "0.14.99"));
        assert!(zig_version_gt("1.0.0", "0.99.99"));
        // A development build of the same release is not newer.
        assert!(!zig_version_gt("0.14.1-dev.1", "0.14.1"));
        assert!(!zig_version_gt("0.14.1", "0.14.1"));
        assert!(!zig_version_gt("0.13.0", "0.14.1"));
        // Missing components count as zero, so 0.14 and 0.14.0 are equal.
        assert!(!zig_version_gt("0.14", "0.14.0"));
        assert_eq!(zig_version_order("0.14", "0.14.0"), Ordering::Equal);
    }
}
