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

//! # FastCDC Content-Defined Large File Chunking
//!
//! Provides content-defined chunking (FastCDC v2020) for files exceeding the threshold
//! (default 4 MB). Large files are sliced into variable-length chunks (min 256 KB, avg 1 MB,
//! max 4 MB).
//!
//! ### Key Capabilities:
//! 1. **Deduplication Across Edits**: Modifying a small section of a 500 MB file only hashes
//!    and stores the affected 1 MB chunk; unchanged chunks retain identical BLAKE3 digests.
//! 2. **Sparse / Sub-Range Streaming**: Tools reading only a subset of a file (e.g. `head -n 10`
//!    or reading the ELF header) fetch only chunk 0 rather than downloading the entire 500 MB file.
//! 3. **Canonical Plain Text Manifest**: Serialized deterministically as:
//!    ```text
//!    CHUNKMANIFEST v1 <total_size_bytes> <whole_file_blake3_hex>
//!    CHUNK <offset> <length> <chunk_digest>
//!    ...
//!    ```

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::ManifestEntry;
use crate::cas::digest::Digest;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// Configuration parameters for the FastCDC chunker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    /// Smallest allowable chunk size in bytes.
    pub min_size: usize,
    /// Target average chunk size in bytes.
    pub avg_size: usize,
    /// Largest allowable chunk size in bytes.
    pub max_size: usize,
    /// File size threshold in bytes above which files are chunked into `ManifestEntry::ChunkedFile`.
    pub threshold_bytes: u64,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            min_size: 256 * 1024,             // 256 KB
            avg_size: 1024 * 1024,            // 1 MB
            max_size: 4 * 1024 * 1024,        // 4 MB
            threshold_bytes: 4 * 1024 * 1024, // 4 MB (equals max_size)
        }
    }
}

impl ChunkerConfig {
    /// Configuration with small parameters designed for unit tests and benchmarks.
    pub fn test_small() -> Self {
        Self {
            min_size: 64,
            avg_size: 256,
            max_size: 1024,
            threshold_bytes: 512,
        }
    }
}

/// A single contiguous chunk record in a `ChunkManifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// Byte offset within the uncompressed file where this chunk begins.
    pub offset: u64,
    /// Length of this chunk in bytes.
    pub length: u64,
    /// BLAKE3 digest of the chunk payload.
    pub digest: Digest,
}

/// Manifest recording the chunk structure of a large file in CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkManifest {
    /// Total uncompressed file size in bytes across all chunks.
    pub total_size_bytes: u64,
    /// 64-hex-character BLAKE3 hash of the entire concatenated file for O(1) equality checks.
    pub whole_file_blake3_hex: String,
    /// Ordered list of contiguous chunk entries.
    pub chunks: Vec<ChunkEntry>,
}

impl ChunkManifest {
    /// Serializes the manifest into a canonical plain-text format for CAS storage.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&format!(
            "CHUNKMANIFEST v1 {} {}\n",
            self.total_size_bytes, self.whole_file_blake3_hex
        ));
        for chunk in &self.chunks {
            out.push_str(&format!(
                "CHUNK {} {} {}\n",
                chunk.offset, chunk.length, chunk.digest
            ));
        }
        out.into_bytes()
    }

    /// Deserializes a canonical plain-text manifest with strict structural validation.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let text = String::from_utf8_lossy(data);
        let mut lines = text.lines();
        let header = lines.next().ok_or_else(|| "Empty chunk manifest".to_string())?;
        let header_parts: Vec<&str> = header.split_whitespace().collect();
        if header_parts.len() < 4 || header_parts[0] != "CHUNKMANIFEST" || header_parts[1] != "v1" {
            return Err(format!("Malformed CHUNKMANIFEST header: {}", header));
        }
        let total_size_bytes = header_parts[2]
            .parse::<u64>()
            .map_err(|e| format!("Invalid total_size in header: {}", e))?;
        let whole_file_blake3_hex = header_parts[3].to_string();

        let mut chunks = Vec::new();
        let mut expected_offset = 0u64;

        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() < 4 || parts[0] != "CHUNK" {
                return Err(format!("Malformed CHUNK line: {}", line));
            }
            let offset = parts[1]
                .parse::<u64>()
                .map_err(|e| format!("Invalid offset: {}", e))?;
            let length = parts[2]
                .parse::<u64>()
                .map_err(|e| format!("Invalid length: {}", e))?;
            let digest = Digest::parse(parts[3])
                .map_err(|e| format!("Invalid chunk digest: {}", e))?;

            if offset != expected_offset {
                return Err(format!(
                    "Non-contiguous chunk offset: expected {}, got {}",
                    expected_offset, offset
                ));
            }
            if length == 0 {
                return Err("Zero-length chunk forbidden".to_string());
            }
            if digest.size_bytes > 0 && digest.size_bytes as u64 != length {
                return Err(format!(
                    "Chunk digest size {} does not match declared length {}",
                    digest.size_bytes, length
                ));
            }

            expected_offset += length;
            chunks.push(ChunkEntry {
                offset,
                length,
                digest,
            });
        }

        if expected_offset != total_size_bytes {
            return Err(format!(
                "Total chunk length {} does not match declared total_size {}",
                expected_offset, total_size_bytes
            ));
        }

        Ok(Self {
            total_size_bytes,
            whole_file_blake3_hex,
            chunks,
        })
    }
}

/// Slices an in-memory byte slice using FastCDC v2020, writes chunks to `store`, and returns `(manifest_digest, manifest)`.
pub fn chunk_slice<B: BlobStore + ?Sized>(
    data: &[u8],
    store: &B,
    config: &ChunkerConfig,
) -> io::Result<(Digest, ChunkManifest)> {
    let mut whole_hasher = blake3::Hasher::new();
    whole_hasher.update(data);
    let whole_file_blake3_hex = whole_hasher.finalize().to_hex().to_string();

    let chunker = fastcdc::v2020::FastCDC::new(
        data,
        config.min_size as u32,
        config.avg_size as u32,
        config.max_size as u32,
    );

    let mut chunks = Vec::new();
    for entry in chunker {
        let chunk_bytes = &data[entry.offset..entry.offset + entry.length];
        let chunk_digest = store.put(chunk_bytes)?;
        chunks.push(ChunkEntry {
            offset: entry.offset as u64,
            length: entry.length as u64,
            digest: chunk_digest,
        });
    }

    let manifest = ChunkManifest {
        total_size_bytes: data.len() as u64,
        whole_file_blake3_hex,
        chunks,
    };

    let manifest_bytes = manifest.serialize();
    let manifest_digest = store.put(&manifest_bytes)?;
    Ok((manifest_digest, manifest))
}

/// Chunks a file from disk using streaming I/O (`StreamCDC`) so large files do not exhaust RAM.
pub fn chunk_file<B: BlobStore + ?Sized, P: AsRef<Path>>(
    path: P,
    store: &B,
    config: &ChunkerConfig,
) -> io::Result<(Digest, ChunkManifest)> {
    let file = std::fs::File::open(path.as_ref())?;
    let total_len = file.metadata()?.len();

    let mut whole_hasher = blake3::Hasher::new();
    let reader = std::io::BufReader::new(file);

    let mut stream_cdc = fastcdc::v2020::StreamCDC::new(
        reader,
        config.min_size as u32,
        config.avg_size as u32,
        config.max_size as u32,
    );

    let mut chunks = Vec::new();
    for chunk_res in &mut stream_cdc {
        let chunk_data =
            chunk_res.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        whole_hasher.update(&chunk_data.data);
        let chunk_digest = store.put(&chunk_data.data)?;
        chunks.push(ChunkEntry {
            offset: chunk_data.offset as u64,
            length: chunk_data.length as u64,
            digest: chunk_digest,
        });
    }

    let whole_file_blake3_hex = whole_hasher.finalize().to_hex().to_string();

    let manifest = ChunkManifest {
        total_size_bytes: total_len,
        whole_file_blake3_hex,
        chunks,
    };

    let manifest_bytes = manifest.serialize();
    let manifest_digest = store.put(&manifest_bytes)?;
    Ok((manifest_digest, manifest))
}

/// Unified helper used by both `graft ingest` and `graft commit` (`apply_changes_to_tree`).
///
/// Stores files $\le \text{threshold}$ as single `ManifestEntry::File` blobs, and files
/// $> \text{threshold}$ as FastCDC `ManifestEntry::ChunkedFile` entries.
pub fn store_file_or_chunked<B: BlobStore + ?Sized, P: AsRef<Path>>(
    path: P,
    name: &str,
    is_executable: bool,
    store: &B,
    config: &ChunkerConfig,
) -> io::Result<ManifestEntry> {
    let path = path.as_ref();
    let meta = std::fs::metadata(path)?;
    let mode = if is_executable { "0755" } else { "0644" }.to_string();

    if meta.len() > config.threshold_bytes {
        let (manifest_digest, _manifest) = chunk_file(path, store, config)?;
        Ok(ManifestEntry::ChunkedFile {
            mode,
            name: name.to_string(),
            manifest_digest,
            total_size: meta.len(),
        })
    } else {
        let bytes = std::fs::read(path)?;
        let digest = store.put(&bytes)?;
        Ok(ManifestEntry::File {
            mode,
            name: name.to_string(),
            digest,
        })
    }
}

/// Reads a byte sub-range `[offset .. offset + size]` from a chunked file.
///
/// Only the chunk(s) overlapping the requested range are accessed or hydrated from L2.
pub fn read_chunked_range<B: BlobStore + ?Sized>(
    manifest: &ChunkManifest,
    offset: u64,
    size: u32,
    store: &B,
) -> io::Result<Vec<u8>> {
    if offset >= manifest.total_size_bytes || size == 0 {
        return Ok(Vec::new());
    }

    let req_end = (offset + size as u64).min(manifest.total_size_bytes);
    let mut out = Vec::with_capacity((req_end - offset) as usize);

    // Locate the first chunk containing bytes at or after `offset` in O(log C)
    let start_idx = manifest
        .chunks
        .partition_point(|c| c.offset + c.length <= offset);

    for chunk in &manifest.chunks[start_idx..] {
        if chunk.offset >= req_end {
            break;
        }

        let chunk_start = chunk.offset;
        let chunk_end = chunk.offset + chunk.length;

        // Intersection between [chunk_start, chunk_end) and [offset, req_end)
        let slice_start = offset.max(chunk_start);
        let slice_end = req_end.min(chunk_end);

        let rel_offset = slice_start - chunk_start;
        let rel_len = (slice_end - slice_start) as usize;

        // Fast path: Zero-copy pread64 from local NVMe cache
        if let Some(path) = store.get_file_path(&chunk.digest) {
            use std::os::unix::fs::FileExt;
            let file = std::fs::File::open(&path)?;
            let mut buf = vec![0u8; rel_len];
            file.read_at(&mut buf, rel_offset)?;
            out.extend_from_slice(&buf);
        } else {
            // Fallback: CAS get() (hydrates single chunk on CachedBlobStore)
            let chunk_data = store.get(&chunk.digest)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chunk blob {} missing from CAS", chunk.digest),
                )
            })?;
            let start = rel_offset as usize;
            let end = start + rel_len;
            out.extend_from_slice(&chunk_data[start..end]);
        }
    }

    Ok(out)
}

/// Streams the entire content of a chunked file into a writer.
pub fn stream_chunked_file<B: BlobStore + ?Sized, W: io::Write>(
    manifest: &ChunkManifest,
    store: &B,
    writer: &mut W,
) -> io::Result<u64> {
    let mut written = 0u64;
    for chunk in &manifest.chunks {
        if let Some(path) = store.get_file_path(&chunk.digest) {
            let mut file = std::fs::File::open(&path)?;
            io::copy(&mut file, writer)?;
        } else {
            let bytes = store.get(&chunk.digest)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Chunk blob {} missing from CAS", chunk.digest),
                )
            })?;
            writer.write_all(&bytes)?;
        }
        written += chunk.length;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use tempfile::tempdir;

    #[test]
    fn test_chunk_manifest_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = ChunkManifest {
            total_size_bytes: 1000,
            whole_file_blake3_hex: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            chunks: vec![
                ChunkEntry {
                    offset: 0,
                    length: 400,
                    digest: Digest::from_bytes(&vec![1u8; 400]),
                },
                ChunkEntry {
                    offset: 400,
                    length: 600,
                    digest: Digest::from_bytes(&vec![2u8; 600]),
                },
            ],
        };

        let bytes = manifest.serialize();
        let parsed = ChunkManifest::from_bytes(&bytes)?;
        assert_eq!(manifest, parsed);

        Ok(())
    }

    #[test]
    fn test_chunk_slice_and_file_parity() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;
        let config = ChunkerConfig::test_small();

        // Generate pseudo-random deterministic buffer
        let mut data = Vec::with_capacity(5000);
        for i in 0..5000 {
            data.push(((i * 37 + 13) % 256) as u8);
        }

        let test_file = tmp.path().join("test.bin");
        std::fs::write(&test_file, &data)?;

        // 1. Chunk via slice
        let (slice_digest, slice_manifest) = chunk_slice(&data, &store, &config)?;

        // 2. Chunk via streaming file
        let (file_digest, file_manifest) = chunk_file(&test_file, &store, &config)?;

        // Assert exact parity!
        assert_eq!(slice_digest, file_digest);
        assert_eq!(slice_manifest, file_manifest);
        assert!(slice_manifest.chunks.len() >= 3);

        // 3. Test sub-range reading
        // Read across boundary: bytes 200..350 (size 150)
        let sub = read_chunked_range(&slice_manifest, 200, 150, &store)?;
        assert_eq!(sub, &data[200..350]);

        // Read first 50 bytes (chunk 0 only)
        let first = read_chunked_range(&slice_manifest, 0, 50, &store)?;
        assert_eq!(first, &data[0..50]);

        // Read beyond EOF
        let eof = read_chunked_range(&slice_manifest, 6000, 100, &store)?;
        assert!(eof.is_empty());

        // 4. Test stream full file
        let mut reassembled = Vec::new();
        stream_chunked_file(&slice_manifest, &store, &mut reassembled)?;
        assert_eq!(reassembled, data);

        Ok(())
    }

    #[test]
    fn test_fastcdc_shift_resistance_deduplication() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;
        let config = ChunkerConfig::test_small();

        // 1. Base buffer (8000 bytes)
        let mut base_data = Vec::with_capacity(8000);
        for i in 0..8000 {
            base_data.push(((i * 73 + 19) % 256) as u8);
        }
        let (_d1, m1) = chunk_slice(&base_data, &store, &config)?;

        // 2. Insert 64 bytes at offset 100 (simulating an edit that shifts all subsequent offsets)
        let mut edited_data = base_data.clone();
        let insert_bytes = vec![0xAA; 64];
        edited_data.splice(100..100, insert_bytes);
        let (_d2, m2) = chunk_slice(&edited_data, &store, &config)?;

        // FastCDC content-defined boundaries ensure downstream chunks are identical!
        let m1_hashes: std::collections::HashSet<Digest> =
            m1.chunks.iter().map(|c| c.digest).collect();
        let shared_chunks = m2.chunks.iter().filter(|c| m1_hashes.contains(&c.digest)).count();

        // At least 60% of chunks must be reused despite the insertion offset shift!
        let reuse_ratio = shared_chunks as f64 / m2.chunks.len() as f64;
        assert!(
            reuse_ratio >= 0.60,
            "Expected FastCDC deduplication ratio >= 0.60, got {:.2}",
            reuse_ratio
        );

        Ok(())
    }
}
