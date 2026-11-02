// Copyright 2026 The Graft Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Content-Addressed Digest Module
//!
//! In Graft, everything (file content, directory manifests, commit snapshots) is
//! identified by a cryptographic `Digest`.
//!
//! ### Why BLAKE3 instead of Git's SHA-1 or SHA-256?
//! - **Throughput**: BLAKE3 sustains > 6 GB/s per core using SIMD instructions,
//!   compared to ~400 MB/s for SHA-256. At agent scale (ingesting 100,000s of files
//!   per minute), hashing cannot be the CPU bottleneck.
//! - **Tree Hashing**: BLAKE3 is inherently a Merkle tree, enabling parallel hashing
//!   of large files and verified streaming of sub-file chunks without re-hashing the whole file.
//!
//! ### Why include `size_bytes` in the Digest?
//! Following the Bazel Remote Execution API (REAPI) standard, pairing the hash with
//! the uncompressed byte length allows the virtual filesystem and network caches to:
//! 1. Pre-allocate buffer sizes and report accurate file attributes (`stat`) without downloading bytes.
//! 2. Detect truncated or corrupted network transfers immediately.

use std::fmt;

/// Length of a BLAKE3-256 hash in bytes (32 bytes = 256 bits).
pub const DIGEST_SIZE: usize = 32;

/// A Content-Addressed Digest uniquely identifying a blob, directory manifest, or commit.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    /// 32-byte cryptographic hash (BLAKE3-256).
    pub hash: [u8; DIGEST_SIZE],
    /// Size of the uncompressed payload in bytes.
    pub size_bytes: usize,
}

impl Digest {
    /// Create a Digest from raw bytes and content length.
    pub fn new(hash: [u8; DIGEST_SIZE], size_bytes: usize) -> Self {
        Self { hash, size_bytes }
    }

    /// Compute a Digest from arbitrary in-memory bytes using BLAKE3.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let hash = blake3::hash(bytes);
        Self {
            hash: *hash.as_bytes(),
            size_bytes: bytes.len(),
        }
    }

    /// Computes a Digest from a file on disk using streaming BLAKE3 hashing.
    pub fn from_file<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 65536];
        let mut size_bytes = 0usize;
        loop {
            use std::io::Read;
            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
            size_bytes += bytes_read;
        }
        let hash = hasher.finalize();
        Ok(Self {
            hash: *hash.as_bytes(),
            size_bytes,
        })
    }

    /// Returns the 64-character lowercase hex string representation of the 32-byte hash.
    pub fn to_hex(&self) -> String {
        hex::encode(self.hash)
    }

    /// Parse a 64-character hex string into a Digest with a known size.
    pub fn from_hex(hex_str: &str, size_bytes: usize) -> Result<Self, String> {
        if hex_str.len() != DIGEST_SIZE * 2 {
            return Err(format!(
                "Invalid hex length: expected {}, got {}",
                DIGEST_SIZE * 2,
                hex_str.len()
            ));
        }
        let mut hash = [0u8; DIGEST_SIZE];
        hex::decode_to_slice(hex_str, &mut hash)
            .map_err(|e| format!("Hex decoding failed: {}", e))?;
        Ok(Self { hash, size_bytes })
    }

    /// Parses a Digest from either a 64-character hex string or `<hex_hash>:<size_bytes>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if let Some((hex_part, size_part)) = trimmed.split_once(':') {
            let size = size_part
                .parse::<usize>()
                .map_err(|e| format!("Invalid size in digest '{}': {}", s, e))?;
            Self::from_hex(hex_part, size)
        } else {
            Self::from_hex(trimmed, 0)
        }
    }

    /// Returns a 2-character prefix (e.g., "38" or "c5") used for sharding files on disk.
    ///
    /// Linux filesystems (ext4/xfs) slow down significantly when a single directory holds
    /// millions of files. Sharding into 256 top-level directories (`00` through `ff`)
    /// keeps directory inode lookups fast.
    pub fn shard_prefix(&self) -> String {
        format!("{:02x}", self.hash[0])
    }

    /// Returns the remaining 62 hex characters after the 2-character shard prefix.
    pub fn shard_suffix(&self) -> String {
        hex::encode(&self.hash[1..])
    }

    /// Returns the first 7 characters of the hexadecimal hash (standard Git-style short SHA).
    pub fn short_hex(&self) -> String {
        let hex = self.to_hex();
        hex[..7.min(hex.len())].to_string()
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({} ({} bytes))", self.to_hex(), self.size_bytes)
    }
}

/// Formatted as `<hex_hash>:<size_bytes>` (e.g. `c54b58c2...:13`).
/// When a precision is specified (e.g. `{:.7}`), formats the truncated hex hash.
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(prec) = f.precision() {
            let hex = self.to_hex();
            write!(f, "{}", &hex[..prec.min(hex.len())])
        } else {
            write!(f, "{}:{}", self.to_hex(), self.size_bytes)
        }
    }
}

impl serde::Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_digest_deterministic() {
        let data = b"hello world from graft!";
        let d1 = Digest::from_bytes(data);
        let d2 = Digest::from_bytes(data);
        assert_eq!(d1, d2);
        assert_eq!(d1.size_bytes, data.len());
    }

    #[test]
    fn test_digest_hex_roundtrip() {
        let data = b"graft cas testing payload";
        let original = Digest::from_bytes(data);
        let hex_str = original.to_hex();
        let parsed = Digest::from_hex(&hex_str, data.len()).unwrap();
        assert_eq!(original, parsed);
    }
}
