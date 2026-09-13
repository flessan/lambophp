//! SHA-256, the digest Lambo uses to verify everything it downloads.
//!
//! Checksum verification is the single most important safety property of the
//! runtime manager: PHP, Apache and MariaDB are *executed*, so a tampered
//! archive must be rejected before a single byte of it runs. The engine
//! therefore needs a digest it can trust on every platform, including a
//! fresh Windows machine with nothing else installed.
//!
//! The implementation below is the FIPS 180-4 algorithm in plain safe Rust -
//! no `unsafe`, no platform-specific intrinsics, no third-party dependency.
//! It is verified against the published NIST vectors in the unit tests, so a
//! mistake shows up as a failing test rather than as a silently accepted
//! download.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::error::{Error, Result};

/// Round constants: the first 32 bits of the fractional parts of the cube
/// roots of the first 64 primes.
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Initial hash values: the first 32 bits of the fractional parts of the
/// square roots of the first 8 primes.
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// Size of one compression block in bytes.
const BLOCK: usize = 64;

/// An incremental SHA-256 computation.
///
/// ```
/// use lambo_core::sha256::Hasher;
///
/// let mut hasher = Hasher::new();
/// hasher.update(b"hello ");
/// hasher.update(b"world");
/// assert_eq!(
///     hasher.finish_hex(),
///     "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
/// );
/// ```
#[derive(Debug, Clone)]
pub struct Hasher {
    state: [u32; 8],
    buffer: [u8; BLOCK],
    buffered: usize,
    length: u64,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    /// Starts a new computation.
    pub fn new() -> Self {
        Self {
            state: H0,
            buffer: [0; BLOCK],
            buffered: 0,
            length: 0,
        }
    }

    /// Feeds more bytes into the digest.
    pub fn update(&mut self, mut input: &[u8]) {
        self.length = self.length.wrapping_add(input.len() as u64);

        if self.buffered > 0 {
            let space = BLOCK - self.buffered;
            let take = space.min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered == BLOCK {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }

        while input.len() >= BLOCK {
            let (block, rest) = input.split_at(BLOCK);
            let mut bytes = [0u8; BLOCK];
            bytes.copy_from_slice(block);
            self.compress(&bytes);
            input = rest;
        }

        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffered = input.len();
        }
    }

    /// Finalizes and returns the raw 32-byte digest.
    pub fn finish(mut self) -> [u8; 32] {
        let bit_length = self.length.wrapping_mul(8);

        // Padding: a single 1 bit, zero bits, then the 64-bit big-endian
        // length. The 0x80 marker always goes in, which is why a message
        // that already fills the buffer needs one more block.
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > BLOCK - 8 {
            let tail = self.buffered;
            for byte in &mut self.buffer[tail..] {
                *byte = 0;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        for byte in &mut self.buffer[self.buffered..BLOCK - 8] {
            *byte = 0;
        }
        self.buffer[BLOCK - 8..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut digest = [0u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// Finalizes and returns the lowercase hex digest.
    pub fn finish_hex(self) -> String {
        to_hex(&self.finish())
    }

    /// One compression round over a single 64-byte block.
    fn compress(&mut self, block: &[u8; BLOCK]) {
        let mut w = [0u32; 64];
        for index in 0..16 {
            w[index] = u32::from_be_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Digests a byte slice.
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(input);
    hasher.finish()
}

/// Digests a byte slice and returns lowercase hex.
pub fn sha256_hex(input: &[u8]) -> String {
    to_hex(&sha256(input))
}

/// Digests a file, reading it in chunks so large archives stay cheap.
///
/// Reads are done through a buffered reader: PHP/Apache archives are tens of
/// megabytes and hashing must not dominate `lambo php install`.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).map_err(|source| Error::io(path, source))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Hasher::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|source| Error::io(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hasher.finish_hex())
}

/// Formats a digest as lowercase hex.
pub fn to_hex(digest: &[u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Normalizes a user- or upstream-supplied checksum.
///
/// Accepts a bare hex digest, `sha256sum`-style `<digest>  <filename>` lines
/// (both the two-space and `*`-prefixed forms) and upper-case input, so a
/// checksum can be pasted straight from a download page or a `.sha256` file.
/// Returns `None` when no plausible digest is present.
pub fn parse_digest(input: &str) -> Option<String> {
    let candidate = input
        .trim()
        .split(|c: char| c.is_whitespace() || c == '*')
        .next()?
        .trim();
    if candidate.len() != 64 || !candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(candidate.to_ascii_lowercase())
}

/// Compares two digests in constant-ish time.
///
/// Not a cryptographic side-channel defence - the digests are public - but
/// comparing the normalized strings keeps the comparison free of
/// short-circuit surprises in error messages.
pub fn digests_match(expected: &str, actual: &str) -> bool {
    match (parse_digest(expected), parse_digest(actual)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    }
}

/// Verifies a file against an expected digest, returning the computed digest.
///
/// This is the only place Lambo decides whether a download may be used; see
/// [`crate::download`] for the caller.
pub fn verify_file(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    let Some(expected) = parse_digest(expected) else {
        return Err(Error::ChecksumMismatch {
            path: path.to_path_buf(),
            expected: expected.to_owned(),
            actual,
        });
    };
    if expected != actual {
        return Err(Error::ChecksumMismatch {
            path: path.to_path_buf(),
            expected,
            actual,
        });
    }
    Ok(())
}

/// Reads a `*.sha256` sidecar file (the form windows.php.net publishes).
///
/// The expected content is a single line: `<hex digest>  <file name>` or
/// just `<hex digest>`.
pub fn parse_digest_file(path: &Path) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::io(path, source)),
    };
    Ok(parse_digest(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// The published FIPS 180-4 / NIST SHA-256 examples.
    #[test]
    fn matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Padding boundaries: 1, 55, 56, 63, 64, 65, 119 and 120 bytes each
    /// take a different path through the length-appending logic (the 64-bit
    /// length needs 8 bytes, so a 56-byte message already spills over).
    #[test]
    fn padding_boundaries_are_correct() {
        let cases: &[(usize, &str)] = &[
            (
                1,
                "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
            ),
            (
                55,
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                56,
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                63,
                "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34",
            ),
            (
                64,
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
            (
                65,
                "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0",
            ),
            (
                119,
                "31eba51c313a5c08226adf18d4a359cfdfd8d2e816b13f4af952f7ea6584dcfb",
            ),
            (
                120,
                "2f3d335432c70b580af0e8e1b3674a7c020d683aa5f73aaaedfdc55af904c21c",
            ),
        ];
        for (len, expected) in cases {
            let input = vec![b'a'; *len];
            assert_eq!(sha256_hex(&input), *expected, "{len}-byte input");

            // The same digest must fall out of the streaming path fed one
            // byte at a time, which is how large archives are hashed.
            let mut hasher = Hasher::new();
            for byte in &input {
                hasher.update(std::slice::from_ref(byte));
            }
            assert_eq!(hasher.finish_hex(), *expected, "{len}-byte input, streamed");
        }
    }

    /// A million bytes must match the published vector; this exercises the
    /// block loop far past the buffering edge cases.
    #[test]
    fn million_byte_vector() {
        let mut hasher = Hasher::new();
        for _ in 0..1000 {
            hasher.update(&vec![b'a'; 1000]);
        }
        assert_eq!(
            hasher.finish_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Feeding bytes one at a time must produce the same digest as one shot.
    #[test]
    fn incremental_updates_match_one_shot() {
        let input: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let mut hasher = Hasher::new();
        for byte in &input {
            hasher.update(std::slice::from_ref(byte));
        }
        assert_eq!(hasher.finish(), sha256(&input));
    }

    #[test]
    fn digests_of_files_match_their_bytes() {
        let temp = TempDir::new();
        let path = temp.path().join("archive.zip");
        std::fs::write(&path, b"php-8.4.0-Win32-vs17-x64.zip").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            sha256_hex(b"php-8.4.0-Win32-vs17-x64.zip")
        );
        assert!(sha256_file(&temp.path().join("missing.zip")).is_err());
    }

    #[test]
    fn verify_file_rejects_mismatches() {
        let temp = TempDir::new();
        let path = temp.path().join("payload.bin");
        std::fs::write(&path, b"abc").unwrap();

        let good = sha256_hex(b"abc");
        verify_file(&path, &good).unwrap();
        // Case and sidecar formatting must not matter.
        verify_file(&path, &good.to_uppercase()).unwrap();
        verify_file(&path, &format!("{good}  payload.bin")).unwrap();

        let err = verify_file(&path, &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));
        assert!(err.to_string().contains("checksum verification failed"));
    }

    #[test]
    fn digest_parsing_accepts_upstream_formats() {
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(parse_digest(digest).as_deref(), Some(digest));
        assert_eq!(
            parse_digest(&format!("{digest} *php.zip\n")).as_deref(),
            Some(digest)
        );
        assert_eq!(
            parse_digest(&format!("  {digest}  php.zip  ")).as_deref(),
            Some(digest)
        );
        assert_eq!(
            parse_digest(&digest.to_uppercase()).as_deref(),
            Some(digest)
        );
        assert_eq!(parse_digest("too-short"), None);
        assert_eq!(parse_digest(""), None);
        assert_eq!(parse_digest(&"z".repeat(64)), None);
    }

    #[test]
    fn digest_files_are_optional() {
        let temp = TempDir::new();
        let path = temp.path().join("php.zip.sha256");
        assert_eq!(parse_digest_file(&path).unwrap(), None);
        std::fs::write(
            &path,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD *php.zip\n",
        )
        .unwrap();
        assert_eq!(
            parse_digest_file(&path).unwrap().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn hex_output_is_lowercase_and_full_length() {
        let hex = sha256_hex(b"lambo");
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex.to_lowercase());
    }
}
