//! BLAKE3 content hashing for files and byte slices.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

const HASH_BYTES: usize = 32;
const HEX_CHARS: usize = HASH_BYTES * 2;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash([u8; HASH_BYTES]);

impl ContentHash {
    pub fn as_bytes(&self) -> &[u8; HASH_BYTES] {
        &self.0
    }

    pub fn from_hex(hex: &str) -> Result<Self, FromHexError> {
        if hex.len() != HEX_CHARS {
            return Err(FromHexError::InvalidLength {
                expected: HEX_CHARS,
                actual: hex.len(),
            });
        }

        let mut bytes = [0u8; HASH_BYTES];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let start = i * 2;
            let pair = hex
                .get(start..start + 2)
                .ok_or(FromHexError::InvalidLength {
                    expected: HEX_CHARS,
                    actual: hex.len(),
                })?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| FromHexError::InvalidChar {
                index: start,
                value: pair.to_owned(),
            })?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({self})")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FromHexError {
    InvalidLength { expected: usize, actual: usize },
    InvalidChar { index: usize, value: String },
}

impl fmt::Display for FromHexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { expected, actual } => write!(
                f,
                "invalid hex length: expected {expected} characters, got {actual}"
            ),
            Self::InvalidChar { index, value } => {
                write!(f, "invalid hex character at index {index}: {value:?}")
            }
        }
    }
}

impl std::error::Error for FromHexError {}

#[derive(Debug)]
pub enum HashError {
    Io(io::Error),
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error while hashing: {err}"),
        }
    }
}

impl std::error::Error for HashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
        }
    }
}

impl From<io::Error> for HashError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    let digest = blake3::hash(bytes);
    ContentHash(*digest.as_bytes())
}

pub fn hash_file(path: &Path) -> Result<ContentHash, HashError> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&mut file)?;
    Ok(ContentHash(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const EMPTY_BLAKE3_HEX: &str =
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    #[test]
    fn hash_bytes_is_deterministic() {
        let a = hash_bytes(b"hello world");
        let b = hash_bytes(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_bytes_matches_known_value_for_empty_input() {
        let h = hash_bytes(b"");
        assert_eq!(h.to_string(), EMPTY_BLAKE3_HEX);
    }

    #[test]
    fn hash_bytes_matches_known_value_for_abc() {
        let h = hash_bytes(b"abc");
        assert_eq!(
            h.to_string(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn display_is_lowercase_hex() {
        let h = hash_bytes(b"");
        let s = h.to_string();
        assert_eq!(s.len(), HEX_CHARS);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    #[test]
    fn debug_format_includes_hex() {
        let h = hash_bytes(b"");
        let dbg = format!("{h:?}");
        assert!(dbg.contains(EMPTY_BLAKE3_HEX));
        assert!(dbg.starts_with("ContentHash("));
    }

    #[test]
    fn from_hex_roundtrips() {
        let h = hash_bytes(b"roundtrip");
        let hex = h.to_string();
        let parsed = ContentHash::from_hex(&hex).expect("valid hex should parse");
        assert_eq!(h, parsed);
        assert_eq!(parsed.as_bytes(), h.as_bytes());
    }

    #[test]
    fn from_hex_rejects_wrong_length() {
        let err = ContentHash::from_hex("deadbeef").expect_err("short hex should fail");
        assert_eq!(
            err,
            FromHexError::InvalidLength {
                expected: HEX_CHARS,
                actual: 8,
            }
        );
    }

    #[test]
    fn from_hex_rejects_invalid_chars() {
        let mut bad = "0".repeat(HEX_CHARS);
        bad.replace_range(0..2, "zz");
        let err = ContentHash::from_hex(&bad).expect_err("non-hex chars should fail");
        match err {
            FromHexError::InvalidChar { index, .. } => assert_eq!(index, 0),
            other => panic!("expected InvalidChar, got {other:?}"),
        }
    }

    #[test]
    fn hash_file_matches_hash_bytes_for_small_file() {
        let mut tmp = NamedTempFile::new().expect("create temp file");
        tmp.write_all(b"some bytes here").expect("write");
        tmp.flush().expect("flush");
        let from_file = hash_file(tmp.path()).expect("hash file");
        let from_bytes = hash_bytes(b"some bytes here");
        assert_eq!(from_file, from_bytes);
    }

    #[test]
    fn hash_file_streams_large_file_without_loading_into_memory() {
        // 4 MiB of pseudo-random-ish but deterministic content so we can hash bytes too.
        let mut buf = Vec::with_capacity(4 * 1024 * 1024);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..(4 * 1024 * 1024) {
            // xorshift32 step
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            buf.push(state as u8);
        }

        let mut tmp = NamedTempFile::new().expect("create temp file");
        tmp.write_all(&buf).expect("write large file");
        tmp.flush().expect("flush");

        let streamed = hash_file(tmp.path()).expect("stream hash");
        let in_memory = hash_bytes(&buf);
        assert_eq!(streamed, in_memory);
    }

    #[test]
    fn hash_file_returns_io_error_for_missing_path() {
        let err = hash_file(Path::new("/this/path/should/not/exist/xgraph-test")).unwrap_err();
        match err {
            HashError::Io(io) => assert_eq!(io.kind(), io::ErrorKind::NotFound),
        }
    }
}
