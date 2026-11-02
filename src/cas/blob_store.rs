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

//! # Content-Addressed Blob Storage Engine
//!
//! This module implements the physical storage layer for blobs and manifests.
//!
//! ### Core Guarantees of Content-Addressed Storage (CAS):
//! 1. **Immutability**: A blob's path is its cryptographic hash. Once written,
//!    its contents can NEVER change. This eliminates cache invalidation problems
//!    entirely—caches can be retained indefinitely.
//! 2. **Deduplication**: If 50 agents or 10 branches share the same file or chunk,
//!    it is stored exactly once on disk.
//! 3. **Concurrency Safety**: Multiple agent threads writing the same file simultaneously
//!    converge on the exact same hash and content. Atomic POSIX file renames ensure
//!    no reader ever observes a partially written file.

use crate::cas::digest::Digest;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Metadata summary for a stored blob, used during garbage collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    /// Content-addressed Digest of the blob.
    pub digest: Digest,
    /// Physical file size in bytes on disk.
    pub size_bytes: u64,
    /// Last modification timestamp on disk (or cloud update timestamp).
    pub modified_at: Option<std::time::SystemTime>,
}

/// Trait defining the storage interface for Content-Addressed Storage.
///
/// Designed to be implemented by:
/// - `LocalBlobStore`: Local NVMe SSD directory cache (implemented below).
/// - `RemoteBlobStore`: Upstream gRPC client connected to S3/GCS or a shared cluster cache.
pub trait BlobStore: Send + Sync {
    /// Ingests raw data bytes into the CAS.
    ///
    /// Computes the BLAKE3 digest, writes the content if not already present,
    /// and returns the canonical Digest.
    fn put(&self, data: &[u8]) -> io::Result<Digest>;

    /// Retrieves raw blob bytes by Digest.
    ///
    /// Returns `Ok(None)` if the blob is not cached locally.
    /// Returns an error if the content on disk fails hash verification.
    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>>;

    /// Checks if a blob exists in the local store without loading its bytes into memory.
    fn has(&self, digest: &Digest) -> bool;

    /// Returns the absolute on-disk file path for zero-copy reading via the OS page cache.
    fn get_file_path(&self, digest: &Digest) -> Option<PathBuf>;

    /// Lists all blobs currently stored in this repository.
    fn list_blobs(&self) -> io::Result<Vec<BlobInfo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "list_blobs is not supported by this BlobStore implementation",
        ))
    }

    /// Deletes a blob by its Digest. Returns `Ok(true)` if deleted, `Ok(false)` if not found.
    fn delete(&self, _digest: &Digest) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "delete is not supported by this BlobStore implementation",
        ))
    }
}

impl<B: BlobStore + ?Sized> BlobStore for std::sync::Arc<B> {
    fn put(&self, data: &[u8]) -> io::Result<Digest> {
        (**self).put(data)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        (**self).get(digest)
    }

    fn has(&self, digest: &Digest) -> bool {
        (**self).has(digest)
    }

    fn get_file_path(&self, digest: &Digest) -> Option<PathBuf> {
        (**self).get_file_path(digest)
    }

    fn list_blobs(&self) -> io::Result<Vec<BlobInfo>> {
        (**self).list_blobs()
    }

    fn delete(&self, digest: &Digest) -> io::Result<bool> {
        (**self).delete(digest)
    }
}

/// A filesystem-backed local CAS store organized in a 2-character sharded hierarchy.
///
/// ### Disk Layout Example:
/// ```text
/// <root>/
/// ├── 38/
/// │   └── 4307e412eee4b3...   <- Root Directory Manifest
/// ├── c5/
/// │   └── 4b58c223d8a52b...   <- README.md blob bytes
/// └── ea/
///     └── f92b01b36f056f...   <- main.rs blob bytes
/// ```
pub struct LocalBlobStore {
    root_dir: PathBuf,
}

impl LocalBlobStore {
    /// Initialize or open a local blob store at the specified root directory.
    /// Creates the directory if it does not already exist.
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let root_dir = path.as_ref().to_path_buf();
        fs::create_dir_all(&root_dir)?;
        Ok(Self { root_dir })
    }

    /// Computes the sharded path for a given digest: `<root>/<shard_prefix>/<shard_suffix>`
    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.root_dir
            .join(digest.shard_prefix())
            .join(digest.shard_suffix())
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, data: &[u8]) -> io::Result<Digest> {
        let digest = Digest::from_bytes(data);
        let target_path = self.blob_path(&digest);

        // Fast path: If the blob already exists, it is content-addressed and immutable.
        // Refresh mtime so re-referenced blobs get a fresh grace window against GC.
        if target_path.exists() {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .open(&target_path)
                .and_then(|file| {
                    file.set_times(
                        std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                    )
                });
            return Ok(digest);
        }

        // Ensure parent shard directory exists (e.g. `<root>/c5/`)
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // CRITICAL FOR CONCURRENCY:
        // Write to a temporary file first, flush it to disk, and then rename it into place.
        // On POSIX filesystems, rename is atomic. This guarantees that concurrent processes
        // reading from the store will NEVER read a partially written or truncated file.
        let temp_path = target_path.with_extension("tmp");
        {
            let mut file = File::create(&temp_path)?;
            file.write_all(data)?;
            file.flush()?;
        }

        fs::rename(temp_path, target_path)?;
        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        let path = self.blob_path(digest);
        if !path.exists() {
            return Ok(None);
        }

        let mut file = File::open(&path)?;
        let mut buffer = Vec::with_capacity(digest.size_bytes);
        file.read_to_end(&mut buffer)?;

        // Content verification: guarantees bit-rot protection.
        let computed = Digest::from_bytes(&buffer);
        if computed.hash != digest.hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "CAS corruption detected for {}: computed {}",
                    digest, computed
                ),
            ));
        }
        if digest.size_bytes > 0 && computed.size_bytes != digest.size_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "CAS size mismatch for {}: expected {}, computed {}",
                    digest, digest.size_bytes, computed.size_bytes
                ),
            ));
        }

        Ok(Some(buffer))
    }

    fn has(&self, digest: &Digest) -> bool {
        self.blob_path(digest).exists()
    }

    fn get_file_path(&self, digest: &Digest) -> Option<PathBuf> {
        let path = self.blob_path(digest);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    fn list_blobs(&self) -> io::Result<Vec<BlobInfo>> {
        let mut blobs = Vec::new();
        if !self.root_dir.exists() {
            return Ok(blobs);
        }

        // Iterate shard directories (00..ff)
        for entry in fs::read_dir(&self.root_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = entry.file_name();
            let dir_str = dir_name.to_string_lossy();

            // Strictly 2 hex chars
            if dir_str.len() != 2 || !dir_str.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }

            for file_entry in fs::read_dir(&path)? {
                let file_entry = file_entry?;
                let file_name = file_entry.file_name();
                let file_str = file_name.to_string_lossy();

                // Skip atomic temp files (e.g. *.tmp) and non-hex names
                if file_str.ends_with(".tmp") || !file_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }

                // Shard suffix is 62 hex chars (2 + 62 = 64 hex chars = 32 bytes)
                if file_str.len() != 62 {
                    continue;
                }

                let full_hex = format!("{}{}", dir_str, file_str);
                let meta = file_entry.metadata()?;
                let size_bytes = meta.len();
                let modified_at = meta.modified().ok();

                if let Ok(digest) = Digest::from_hex(&full_hex, size_bytes as usize) {
                    blobs.push(BlobInfo {
                        digest,
                        size_bytes,
                        modified_at,
                    });
                }
            }
        }

        Ok(blobs)
    }

    fn delete(&self, digest: &Digest) -> io::Result<bool> {
        let path = self.blob_path(digest);
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_put_get_deduplication() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let sample_text = b"Graft local CAS test string";
        let digest = store.put(sample_text)?;

        assert!(store.has(&digest));

        let retrieved = store.get(&digest)?.expect("Blob must exist");
        assert_eq!(retrieved, sample_text);

        // Putting the exact same data again should succeed idempotently
        let digest2 = store.put(sample_text)?;
        assert_eq!(digest, digest2);

        Ok(())
    }

    #[test]
    fn test_list_blobs_and_delete() -> io::Result<()> {
        let tmp = tempdir()?;
        let store = LocalBlobStore::new(tmp.path())?;

        let d1 = store.put(b"Blob 1 content")?;
        let d2 = store.put(b"Blob 2 content - different")?;

        let blobs = store.list_blobs()?;
        assert_eq!(blobs.len(), 2);
        let hashes: std::collections::HashSet<[u8; 32]> =
            blobs.iter().map(|b| b.digest.hash).collect();
        assert!(hashes.contains(&d1.hash));
        assert!(hashes.contains(&d2.hash));

        // Delete d1
        assert!(store.delete(&d1)?);
        assert!(!store.has(&d1));
        assert!(store.has(&d2));

        // Delete non-existent
        assert!(!store.delete(&d1)?);

        let remaining = store.list_blobs()?;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].digest.hash, d2.hash);

        Ok(())
    }
}
