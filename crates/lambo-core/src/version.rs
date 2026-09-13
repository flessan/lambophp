//! PHP version specifications.
//!
//! Users refer to PHP versions either by *channel* (`stable`, `latest`,
//! `nightly`) or by a constraint (`8.4`, `8.4.2`, `^8.3`, `>=8.1 <8.5`).
//! [`VersionSpec`] unifies both and encodes the PHP-flavored semantics of
//! shorthands: `8.4` means the `8.4.x` release line (a tilde requirement)
//! and `8.4.2` means exactly that patch release.
//!
//! Channels are *resolved* to concrete versions by the runtime manager
//! against its version manifest; everything before that point (config files,
//! lambofiles, the CLI) speaks in `VersionSpec`s.

use std::fmt;
use std::str::FromStr;

use semver::{Version, VersionReq};
use serde::Deserialize;

use crate::error::{Error, Result};

/// The `stable` channel: newest generally-available release.
pub const STABLE: &str = "stable";
/// The `latest` channel: newest known release of any kind.
pub const LATEST: &str = "latest";
/// The `nightly` channel: development snapshots.
pub const NIGHTLY: &str = "nightly";

/// A user-facing PHP version specification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VersionSpec {
    /// Newest generally-available release.
    Stable,
    /// Newest known release regardless of stability.
    Latest,
    /// Development snapshots.
    Nightly,
    /// A semantic-version requirement such as `~8.4` or `=8.4.2`.
    Req(VersionReq),
}

impl VersionSpec {
    /// `true` when the spec is a concrete requirement rather than a channel.
    pub fn is_req(&self) -> bool {
        matches!(self, Self::Req(_))
    }

    /// Tests a concrete version against this spec.
    ///
    /// Channels match nothing here: they must be resolved to a concrete
    /// requirement first (by the runtime manager, against the manifest).
    pub fn matches(&self, version: &Version) -> bool {
        match self {
            Self::Req(req) => req.matches(version),
            Self::Stable | Self::Latest | Self::Nightly => false,
        }
    }
}

impl Default for VersionSpec {
    fn default() -> Self {
        Self::Stable
    }
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => f.write_str(STABLE),
            Self::Latest => f.write_str(LATEST),
            Self::Nightly => f.write_str(NIGHTLY),
            Self::Req(req) => write!(f, "{req}"),
        }
    }
}

impl FromStr for VersionSpec {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        let input = input.trim();
        match input.to_ascii_lowercase().as_str() {
            STABLE => return Ok(Self::Stable),
            LATEST => return Ok(Self::Latest),
            NIGHTLY => return Ok(Self::Nightly),
            _ => {}
        }
        let normalized = normalize(input);
        VersionReq::parse(&normalized)
            .map(Self::Req)
            .map_err(|e| Error::InvalidVersionSpec {
                input: input.to_owned(),
                reason: e.to_string(),
            })
    }
}

/// Translates PHP-flavored shorthands into semver requirements:
///
/// - `8.4`   -> `~8.4`   (the 8.4.x release line)
/// - `8.4.2` -> `=8.4.2` (exactly this patch release)
/// - anything else passes through unchanged (`^8.3`, `>=8.1 <8.5`, `8.3.*`)
fn normalize(input: &str) -> String {
    let input = normalize_separators(input);
    let parts: Vec<&str> = input.split('.').collect();
    let numeric = parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    match (numeric, parts.len()) {
        (true, 2) => format!("~{input}"),
        (true, 3) => format!("={input}"),
        _ => input.to_owned(),
    }
}

/// Rewrites space-separated comparators into the comma form semver expects.
///
/// `>=8.1 <8.5` is how people write a range; `VersionReq` wants
/// `>=8.1, <8.5`. Spaces *inside* one comparator (`>= 8.1`) are preserved.
fn normalize_separators(input: &str) -> String {
    if input.contains(',') {
        return input.to_owned();
    }
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.len() < 2 {
        return input.to_owned();
    }

    let mut normalized = String::with_capacity(input.len() + tokens.len());
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            let previous_is_operator = tokens[index - 1]
                .chars()
                .all(|c| matches!(c, '<' | '>' | '=' | '^' | '~'));
            let starts_comparator =
                token.starts_with(['<', '>', '=', '^', '~']) || token.starts_with('*');
            normalized.push_str(if starts_comparator && !previous_is_operator {
                ", "
            } else {
                " "
            });
        }
        normalized.push_str(token);
    }
    normalized
}

impl serde::Serialize for VersionSpec {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VersionSpec {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn channels_parse_case_insensitively() {
        assert_eq!(
            "stable".parse::<VersionSpec>().unwrap(),
            VersionSpec::Stable
        );
        assert_eq!(
            "LATEST".parse::<VersionSpec>().unwrap(),
            VersionSpec::Latest
        );
        assert_eq!(
            " nightly ".parse::<VersionSpec>().unwrap(),
            VersionSpec::Nightly
        );
        assert_eq!(VersionSpec::default(), VersionSpec::Stable);
    }

    #[test]
    fn bare_minor_means_release_line() {
        let spec: VersionSpec = "8.4".parse().unwrap();
        assert_eq!(spec.to_string(), "~8.4");
        assert!(spec.matches(&v("8.4.0")));
        assert!(spec.matches(&v("8.4.7")));
        assert!(!spec.matches(&v("8.5.0")));
        assert!(!spec.matches(&v("8.3.99")));
    }

    #[test]
    fn bare_triplet_means_exact() {
        let spec: VersionSpec = "8.4.2".parse().unwrap();
        assert_eq!(spec.to_string(), "=8.4.2");
        assert!(spec.matches(&v("8.4.2")));
        assert!(!spec.matches(&v("8.4.3")));
    }

    #[test]
    fn full_semver_expressions_pass_through() {
        let spec: VersionSpec = "^8.3".parse().unwrap();
        assert!(spec.matches(&v("8.9.0")));
        assert!(!spec.matches(&v("9.0.0")));

        let range: VersionSpec = ">=8.1 <8.5".parse().unwrap();
        assert!(range.matches(&v("8.2.0")));
        assert!(!range.matches(&v("8.5.0")));

        let wildcard: VersionSpec = "8.3.*".parse().unwrap();
        assert!(wildcard.matches(&v("8.3.15")));
        assert!(!wildcard.matches(&v("8.4.0")));
    }

    #[test]
    fn channels_match_nothing_until_resolved() {
        assert!(!VersionSpec::Stable.matches(&v("8.4.2")));
        assert!(!VersionSpec::Nightly.is_req());
        assert!(VersionSpec::Req(VersionReq::parse("~8.4").unwrap()).is_req());
    }

    #[test]
    fn invalid_input_reports_the_offender() {
        let err = "eight.point.four".parse::<VersionSpec>().unwrap_err();
        assert!(err.to_string().contains("eight.point.four"));
        assert!("".parse::<VersionSpec>().is_err());
    }

    #[test]
    fn serde_roundtrips_through_yaml_strings() {
        // Roundtrip rather than exact emission: the emitter may quote
        // `~8.4` defensively, and both spellings must parse back.
        let spec: VersionSpec = "8.4".parse().unwrap();
        let text = crate::yaml::to_string(&spec).unwrap();
        let back: VersionSpec = crate::yaml::from_str(&text).unwrap();
        assert_eq!(back, spec);

        let stable: VersionSpec = crate::yaml::from_str("stable").unwrap();
        assert_eq!(stable, VersionSpec::Stable);
        assert!(crate::yaml::from_str::<VersionSpec>("nope...").is_err());
    }
}
