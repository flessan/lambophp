//! Crate-internal YAML (de)serialization helpers.
//!
//! The YAML backend is deliberately hidden from the rest of the codebase so
//! it can be swapped without touching any call site; see
//! `docs/adr/0004-yaml-backend.md`. Nothing outside `yaml.rs`,
//! `error.rs` and the (de)serialization call sites should name the backend.

/// Serializes a value to a YAML string.
pub(crate) fn to_string<T: serde::Serialize>(value: &T) -> Result<String, serde_yaml::Error> {
    serde_yaml::to_string(value)
}

/// Deserializes a value from a YAML string.
pub(crate) fn from_str<'a, T: serde::Deserialize<'a>>(
    input: &'a str,
) -> Result<T, serde_yaml::Error> {
    serde_yaml::from_str(input)
}
