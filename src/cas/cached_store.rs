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

//! # Tiered Cached Content-Addressed Storage
//!
//! Provides a two-tier storage hierarchy combining a fast local NVMe/SSD cache (L1)
//! with a durable remote object store like Google Cloud Storage (L2).
//!
//! ### Key Capabilities:
//! 1. **Zero-Copy Local Reads**: If a blob is cached in L1, VFS and FUSE access it
//!    directly through the OS page cache at native NVMe speeds.
//! 2. **On-Demand Lazy Hydration**: When the VFS or FUSE asks for a file via
//!    [`BlobStore::get_file_path`], a cache miss in L1 automatically streams the
//!    blob from GCS (L2), validates its BLAKE3 hash, writes it to L1, and returns
//!    the local on-disk path.
//! 3. **Write-Through Persistence**: Writes are committed to both local disk and
//!    remote GCS, ensuring durability across distributed agent workers.

use crate::cas::blob_store::{BlobInfo, BlobStore, LocalBlobStore};
use crate::cas::digest::Digest;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

/// A two-tier Content-Addressed Storage engine.
///
/// - **L1**: Local filesystem cache ([`LocalBlobStore`])
/// - **L2**: Remote persistent store (e.g. [`crate::cas::gcs::GcsBlobStore`])
pub struct CachedBlobStore {
    l1: Arc<LocalBlobStore>,
    l2: Arc<dyn BlobStore>,
}

impl CachedBlobStore {
    /// Creates a new tiered cached store.
    pub fn new(l1: Arc<LocalBlobStore>, l2: Arc<dyn BlobStore>) -> Self {
        Self { l1, l2 }
    }

    /// Access the underlying L1 local store.
    pub fn l1(&self) -> &Arc<LocalBlobStore> {
        &self.l1
    }

    /// Access the underlying L2 remote store.
    pub fn l2(&self) -> &Arc<dyn BlobStore> {
        &self.l2
    }
}

impl BlobStore for CachedBlobStore {
    fn put(&self, data: &[u8]) -> io::Result<Digest> {
        // 1. Write atomically to local L1 cache
        let digest = self.l1.put(data)?;

        // 2. Write-through to remote L2 store
        if let Err(e) = self.l2.put(data) {
            tracing::warn!("Failed to write-through blob {} to remote L2: {}", digest, e);
            // Return Ok because the blob is safely persisted locally,
            // or propagate if strict remote durability is required.
            return Err(e);
        }

        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        // Fast path: Check L1 local NVMe cache
        if let Some(local_data) = self.l1.get(digest)? {
            return Ok(Some(local_data));
        }

        // Cache miss: Fetch from remote L2 (GCS)
        if let Some(remote_data) = self.l2.get(digest)? {
            // Verify integrity before inserting into L1
            let computed = Digest::from_bytes(&remote_data);
            if computed.hash != digest.hash {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "L2 download corruption for {}: expected {}, computed {}",
                        digest, digest, computed
                    ),
                ));
            }

            // Hydrate into L1 cache
            self.l1.put(&remote_data)?;
            return Ok(Some(remote_data));
        }

        Ok(None)
    }

    fn has(&self, digest: &Digest) -> bool {
        self.l1.has(digest) || self.l2.has(digest)
    }

    fn get_file_path(&self, digest: &Digest) -> Option<PathBuf> {
        // If already cached locally, return the on-disk path
        if let Some(path) = self.l1.get_file_path(digest) {
            return Some(path);
        }

        // Lazy hydration: stream blob from L2 into L1
        match self.get(digest) {
            Ok(Some(_)) => self.l1.get_file_path(digest),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("Lazy hydration failed for {}: {}", digest, e);
                None
            }
        }
    }

    fn list_blobs(&self) -> io::Result<Vec<BlobInfo>> {
        self.l1.list_blobs()
    }

    fn delete(&self, digest: &Digest) -> io::Result<bool> {
        self.l1.delete(digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::gcs::{GcsBlobStore, GcsConfig, MockGcsTransport};
    use tempfile::tempdir;

    #[test]
    fn test_cached_blob_store_hydration() -> io::Result<()> {
        let tmp = tempdir()?;
        let l1 = Arc::new(LocalBlobStore::new(tmp.path().join("l1_cache"))?);

        let config = GcsConfig::parse("gs://test-bucket/cas").unwrap();
        let mock_transport = Arc::new(MockGcsTransport::new());
        let l2 = Arc::new(GcsBlobStore::with_transport(config, mock_transport));

        let cached_store = CachedBlobStore::new(l1.clone(), l2.clone());

        // 1. Put directly into L2 (simulating another agent pushing to GCS)
        let sample = b"Remote GCS blob for agent lazy-hydration";
        let digest = l2.put(sample)?;

        // Ensure L1 does NOT have it yet
        assert!(!l1.has(&digest));
        assert!(l1.get_file_path(&digest).is_none());

        // 2. Query through CachedBlobStore: should hydrate from L2 into L1
        assert!(cached_store.has(&digest));

        let path = cached_store
            .get_file_path(&digest)
            .expect("Hydration must return a local file path");
        assert!(path.exists());
        assert!(l1.has(&digest));

        let read_bytes = std::fs::read(&path)?;
        assert_eq!(read_bytes, sample);

        Ok(())
    }
}
