//! Reading a value a previous writer stored as `null`.
//!
//! `#[serde(default)]` covers a *missing* key, not one that is present and
//! null. That distinction matters because the two writers this crate has to
//! read are not Rust: Go's `encoding/json` marshals a nil slice as `null`, and
//! a hand-edited YAML file can leave a list key standing with nothing after
//! it. Both mean "empty", and neither is a reason to reject the file.
//!
//! Rejecting one is not a theoretical failure. The configuration the previous
//! implementation writes
//! on its first launch contains `"projects": null`, because its project list is
//! an empty slice and the field carries no `omitempty`; refusing to parse that
//! would make this implementation unable to open the installation state of the
//! one it replaces.
//!
//! The helper is generic over the deserializer, so the same attribute serves
//! the JSON state file and the YAML ones.

use serde::Deserialize;

/// Deserializes a value that may be `null`, as its default in that case.
///
/// Used through `#[serde(default, deserialize_with = "null_is_empty")]`, which
/// covers both spellings of "empty": the key is absent, or it is null.
pub(crate) fn null_is_empty<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::null_is_empty;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Holder {
        #[serde(default, deserialize_with = "null_is_empty")]
        names: Vec<String>,
        #[serde(default, deserialize_with = "null_is_empty")]
        map: std::collections::BTreeMap<String, u32>,
    }

    #[test]
    fn a_null_list_reads_as_an_empty_list() {
        let parsed: Holder = serde_json::from_str(r#"{"names": null, "map": null}"#).unwrap();
        assert!(parsed.names.is_empty());
        assert!(parsed.map.is_empty());
    }

    #[test]
    fn a_missing_key_and_a_present_one_behave_the_same() {
        let absent: Holder = serde_json::from_str("{}").unwrap();
        let null: Holder = serde_json::from_str(r#"{"names": null}"#).unwrap();
        assert_eq!(absent, null);
    }

    #[test]
    fn real_values_are_untouched() {
        let parsed: Holder = serde_json::from_str(r#"{"names": ["a"], "map": {"b": 2}}"#).unwrap();
        assert_eq!(parsed.names, ["a"]);
        assert_eq!(parsed.map.get("b"), Some(&2));
    }

    #[test]
    fn a_wrong_type_is_still_an_error() {
        // Tolerating null must not turn the loader into one that accepts
        // anything: a number where a list belongs is still a broken file.
        assert!(serde_json::from_str::<Holder>(r#"{"names": 3}"#).is_err());
    }
}
