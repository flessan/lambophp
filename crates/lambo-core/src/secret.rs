//! Generation of the local development credentials Lambo creates.
//!
//! # Threat model, stated plainly
//!
//! Lambo's database listens on `127.0.0.1` only and its password lives in
//! `$LAMBO_HOME/config/lambo.yml` - a file any local user with access to the
//! profile directory can read. The password therefore does *not* protect
//! against a local attacker; it exists so that
//!
//! - no credential is ever hardcoded in this repository or in a release
//!   binary,
//! - another tool on the machine cannot connect by guessing `root` / empty,
//! - two Lambo installations on one machine do not share credentials.
//!
//! Anyone who needs a genuinely secret database password should set one with
//! `lambo config set database.password <value>` and keep it elsewhere.
//!
//! # Entropy sources
//!
//! Everything below is available through safe, cross-platform `std` APIs:
//! the operating-system-seeded key behind [`RandomState`], high-resolution
//! clock readings, the process identifier, and the host name environment
//! variable (`COMPUTERNAME` on Windows, `HOSTNAME` on Unix). The mixture is
//! digested with SHA-256 so the output length is fixed and uniform.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sha256::Hasher as Sha256;

/// Alphabet used for generated tokens: unambiguous, safe in URLs, `.env`
/// files, connection strings and shell command lines on every platform.
const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generates a random token of `length` characters from [`ALPHABET`].
///
/// `length` is clamped to at least 16: a development password shorter than
/// that has no upside.
pub fn token(length: usize) -> String {
    let length = length.max(16);
    let entropy = entropy_bytes();
    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(&entropy);
        hasher.finish()
    };

    // Each digest byte maps to one alphabet character; 32 characters of
    // output come straight from the digest, and longer requests re-hash with
    // a counter so no character repeats a pattern.
    let mut output = String::with_capacity(length);
    let mut round = 0u8;
    while output.len() < length {
        let mut hasher = Sha256::new();
        hasher.update(&digest);
        hasher.update(&[round]);
        for byte in hasher.finish() {
            if output.len() == length {
                break;
            }
            let index = (byte as usize) % ALPHABET.len();
            output.push(ALPHABET[index] as char);
        }
        round = round.wrapping_add(1);
    }
    output
}

/// Default length of a generated database password.
pub const DEFAULT_PASSWORD_LENGTH: usize = 24;

/// Generates a database password of the default length.
pub fn database_password() -> String {
    token(DEFAULT_PASSWORD_LENGTH)
}

/// Collects the entropy available without platform-specific code.
fn entropy_bytes() -> Vec<u8> {
    let mut buffer = Vec::with_capacity(64);

    // OS-seeded per-process keys. Two independent states are hashed so the
    // process-local counter component does not dominate.
    for state in [RandomState::new(), RandomState::new()] {
        let mut hasher = state.build_hasher();
        hasher.write(b"lambo-php");
        buffer.extend_from_slice(&hasher.finish().to_le_bytes());
    }

    if let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) {
        buffer.extend_from_slice(&elapsed.as_nanos().to_le_bytes());
    }
    buffer.extend_from_slice(&std::process::id().to_le_bytes());

    for key in [
        "COMPUTERNAME",
        "HOSTNAME",
        "USERPROFILE",
        "HOME",
        "USERNAME",
        "USER",
    ] {
        if let Some(value) = std::env::var_os(key) {
            buffer.extend_from_slice(value.to_string_lossy().as_bytes());
            buffer.push(0);
        }
    }

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_requested_length_and_alphabet() {
        for length in [16, 24, 32, 64] {
            let value = token(length);
            assert_eq!(value.len(), length);
            assert!(value.bytes().all(|byte| ALPHABET.contains(&byte)));
        }
    }

    #[test]
    fn short_requests_are_raised_to_the_minimum() {
        assert_eq!(token(1).len(), 16);
        assert_eq!(token(0).len(), 16);
    }

    #[test]
    fn two_calls_differ() {
        // The clock advances between calls, so identical output would mean
        // the generator ignored its entropy entirely.
        assert_ne!(token(24), token(24));
        assert_ne!(database_password(), database_password());
    }

    #[test]
    fn tokens_are_safe_for_env_files_and_command_lines() {
        let value = database_password();
        assert!(
            !value
                .chars()
                .any(|c| matches!(c, ' ' | '"' | '\'' | '\\' | '&' | '|' | '%' | '$'))
        );
        assert!(value.len() >= 16);
    }
}
