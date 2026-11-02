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

//! # Google Cloud Storage (GCS) Remote CAS Backend
//!
//! Provides a production-grade Content-Addressed Storage implementation backed by
//! Google Cloud Storage (`gs://<bucket>/<prefix>`).
//!
//! ### Key Features:
//! 1. **Content Addressing & Immutability**:
//!    Blobs are stored at `<prefix>/blobs/<shard_prefix>/<shard_suffix>`.
//!    Uploads use `ifGenerationMatch=0` so existing blobs are never re-written,
//!    and HTTP 412 (Precondition Failed) is treated as an instantaneous cache hit.
//! 2. **Bit-Rot & Transfer Integrity**:
//!    All downloaded blobs are verified against their expected BLAKE3 digest and
//!    byte length before being accepted.
//! 3. **Smart In-Memory Token Caching**:
//!    OAuth access tokens obtained from `gcloud auth print-access-token` or environment
//!    variables (`GRAFT_GCS_TOKEN`) are cached for 45 minutes with automatic invalidation on 401.
//! 4. **Pluggable Transport**:
//!    Includes [`UreqGcsTransport`] for live HTTPS communication with `storage.googleapis.com`
//!    and [`MockGcsTransport`] for fast, hermetic, offline unit testing.

use crate::cas::blob_store::BlobStore;
use crate::cas::digest::Digest;
use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Configuration for Google Cloud Storage bucket and key prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcsConfig {
    /// GCP bucket name (e.g. "my-cas-bucket")
    pub bucket: String,
    /// Optional prefix inside the bucket (e.g. "graft-test" or "cas")
    pub prefix: String,
    /// API Endpoint host (default: "`https://storage.googleapis.com`")
    pub endpoint: String,
}

impl GcsConfig {
    /// Parses a `gs://<bucket>[/<prefix>]` URL string.
    ///
    /// # Examples
    /// ```
    /// use graft::cas::gcs::GcsConfig;
    ///
    /// let config = GcsConfig::parse("gs://my-bucket/cas-data").unwrap();
    /// assert_eq!(config.bucket, "my-bucket");
    /// assert_eq!(config.prefix, "cas-data");
    ///
    /// let root_config = GcsConfig::parse("gs://my-bucket").unwrap();
    /// assert_eq!(root_config.bucket, "my-bucket");
    /// assert_eq!(root_config.prefix, "");
    /// ```
    pub fn parse(url_str: &str) -> io::Result<Self> {
        let trimmed = url_str.trim();
        if !trimmed.starts_with("gs://") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid GCS URL '{}': must start with 'gs://'", url_str),
            ));
        }

        let without_scheme = &trimmed["gs://".len()..];
        let mut parts = without_scheme.splitn(2, '/');
        let bucket = parts.next().unwrap_or("").trim().to_string();
        if bucket.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid GCS URL '{}': missing bucket name", url_str),
            ));
        }

        let raw_prefix = parts.next().unwrap_or("").trim();
        // Strip leading and trailing slashes from prefix
        let prefix = raw_prefix
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();

        let endpoint = env::var("STORAGE_EMULATOR_HOST")
            .unwrap_or_else(|_| "https://storage.googleapis.com".to_string());

        Ok(Self {
            bucket,
            prefix,
            endpoint,
        })
    }

    /// Computes the full object path inside the bucket for a given digest.
    pub fn object_key_for_digest(&self, digest: &Digest) -> String {
        let shard_prefix = digest.shard_prefix();
        let shard_suffix = digest.shard_suffix();
        if self.prefix.is_empty() {
            format!("blobs/{}/{}", shard_prefix, shard_suffix)
        } else {
            format!("{}/blobs/{}/{}", self.prefix, shard_prefix, shard_suffix)
        }
    }
}

/// Helper function to perform RFC 3986 percent-encoding for URI path components.
pub fn percent_encode(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(b as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", b));
            }
        }
    }
    encoded
}

/// Token provider that caches GCP OAuth access tokens in memory.
#[derive(Debug, Default)]
pub struct GcsTokenProvider {
    cached: RwLock<Option<(String, Instant)>>,
}

impl GcsTokenProvider {
    /// Creates a new token provider.
    pub fn new() -> Self {
        Self {
            cached: RwLock::new(None),
        }
    }

    /// Invalidates the currently cached token (e.g. after receiving a 401 Unauthorized).
    pub fn invalidate(&self) {
        let mut lock = self.cached.write().unwrap();
        *lock = None;
    }

    /// Obtains a valid bearer token, using the in-memory cache if available.
    pub fn get_token(&self) -> io::Result<String> {
        // 1. Check in-memory cache (valid for 45 minutes)
        {
            let lock = self.cached.read().unwrap();
            if let Some((token, created_at)) = &*lock {
                if created_at.elapsed() < Duration::from_secs(45 * 60) {
                    return Ok(token.clone());
                }
            }
        }

        // 2. Check environment variable override
        if let Ok(token) = env::var("GRAFT_GCS_TOKEN").or_else(|_| env::var("GOOGLE_OAUTH_ACCESS_TOKEN")) {
            let clean = token.trim().to_string();
            if !clean.is_empty() {
                let mut lock = self.cached.write().unwrap();
                *lock = Some((clean.clone(), Instant::now()));
                return Ok(clean);
            }
        }

        // 3. Fall back to gcloud auth print-access-token
        let output = Command::new("gcloud")
            .args(["auth", "print-access-token"])
            .output()
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Failed to execute 'gcloud auth print-access-token': {}. Please set GRAFT_GCS_TOKEN.", e),
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("'gcloud auth print-access-token' failed: {}. Set GRAFT_GCS_TOKEN or run 'gcloud auth login'.", stderr),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Extract the line containing the bearer token (typically starting with ya29.)
        let token = stdout
            .lines()
            .map(|l| l.trim())
            .find(|l| l.starts_with("ya29."))
            .or_else(|| {
                // If not starting with ya29, pick the first non-empty line that does not start with WARNING
                stdout
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty() && !l.starts_with("WARNING") && !l.starts_with("To take a quick"))
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Failed to parse access token from gcloud output.",
                )
            })?
            .to_string();

        let mut lock = self.cached.write().unwrap();
        *lock = Some((token.clone(), Instant::now()));
        Ok(token)
    }
}

/// Abstract transport layer for GCS HTTP operations.
pub trait GcsTransport: Send + Sync {
    /// Retrieves object bytes. Returns `Ok(None)` if the object does not exist (404).
    fn get_object(&self, bucket: &str, object_name: &str) -> io::Result<Option<Vec<u8>>>;

    /// Uploads object bytes if not already present (`ifGenerationMatch=0`).
    /// Precondition failures (HTTP 412) are treated as successful idempotent writes.
    fn put_object_if_absent(&self, bucket: &str, object_name: &str, data: &[u8]) -> io::Result<()>;

    /// Checks if an object exists in GCS.
    fn head_object(&self, bucket: &str, object_name: &str) -> io::Result<bool>;

    /// Deletes an object from GCS. Returns `Ok(true)` if deleted, `Ok(false)` if not found.
    fn delete_object(&self, bucket: &str, object_name: &str) -> io::Result<bool> {
        let _ = (bucket, object_name);
        Ok(false)
    }
}

/// Production GCS HTTP Transport using `ureq` and live REST JSON API endpoints.
pub struct UreqGcsTransport {
    endpoint: String,
    token_provider: Arc<GcsTokenProvider>,
}

impl UreqGcsTransport {
    /// Creates a new transport targeting the specified endpoint.
    pub fn new(endpoint: &str, token_provider: Arc<GcsTokenProvider>) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token_provider,
        }
    }
}

impl GcsTransport for UreqGcsTransport {
    fn get_object(&self, bucket: &str, object_name: &str) -> io::Result<Option<Vec<u8>>> {
        let encoded_name = percent_encode(object_name);
        let url = format!(
            "{}/download/storage/v1/b/{}/o/{}?alt=media",
            self.endpoint, bucket, encoded_name
        );

        let token = self.token_provider.get_token()?;
        let res = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .call();

        match res {
            Ok(mut response) => {
                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(1024 * 1024 * 1024) // 1 GB limit
                    .read_to_vec()
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Download read error: {}", e)))?;
                Ok(Some(bytes))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(401)) => {
                self.token_provider.invalidate();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GCS returned 401 Unauthorized. Access token expired.",
                ))
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("GCS GET object '{}' failed: {}", object_name, e),
            )),
        }
    }

    fn put_object_if_absent(&self, bucket: &str, object_name: &str, data: &[u8]) -> io::Result<()> {
        let encoded_name = percent_encode(object_name);
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}&ifGenerationMatch=0",
            self.endpoint, bucket, encoded_name
        );

        let token = self.token_provider.get_token()?;
        let res = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .send(data);

        match res {
            Ok(_) => Ok(()),
            // HTTP 412: Object already exists (generation match condition failed).
            // For Content-Addressed Storage, content is identical and immutable!
            Err(ureq::Error::StatusCode(412)) => Ok(()),
            Err(ureq::Error::StatusCode(401)) => {
                self.token_provider.invalidate();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GCS returned 401 Unauthorized during upload. Access token expired.",
                ))
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("GCS upload for object '{}' failed: {}", object_name, e),
            )),
        }
    }

    fn head_object(&self, bucket: &str, object_name: &str) -> io::Result<bool> {
        let encoded_name = percent_encode(object_name);
        let url = format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint, bucket, encoded_name
        );

        let token = self.token_provider.get_token()?;
        let res = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .call();

        match res {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(ureq::Error::StatusCode(401)) => {
                self.token_provider.invalidate();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GCS returned 401 Unauthorized during metadata check.",
                ))
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("GCS metadata check for object '{}' failed: {}", object_name, e),
            )),
        }
    }

    fn delete_object(&self, bucket: &str, object_name: &str) -> io::Result<bool> {
        let encoded_name = percent_encode(object_name);
        let url = format!("{}/storage/v1/b/{}/o/{}", self.endpoint, bucket, encoded_name);
        let token = self.token_provider.get_token()?;

        let res = ureq::delete(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .call();

        match res {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(ureq::Error::StatusCode(401)) => {
                self.token_provider.invalidate();
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "GCS returned 401 Unauthorized during delete.",
                ))
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("GCS delete for object '{}' failed: {}", object_name, e),
            )),
        }
    }
}

/// In-memory mock transport for hermetic, offline testing.
#[derive(Debug, Default)]
pub struct MockGcsTransport {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MockGcsTransport {
    /// Creates an empty in-memory mock GCS transport.
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn key(bucket: &str, object_name: &str) -> String {
        format!("{}/{}", bucket, object_name)
    }
}

impl GcsTransport for MockGcsTransport {
    fn get_object(&self, bucket: &str, object_name: &str) -> io::Result<Option<Vec<u8>>> {
        let map = self.objects.lock().unwrap();
        Ok(map.get(&Self::key(bucket, object_name)).cloned())
    }

    fn put_object_if_absent(&self, bucket: &str, object_name: &str, data: &[u8]) -> io::Result<()> {
        let mut map = self.objects.lock().unwrap();
        let k = Self::key(bucket, object_name);
        if map.contains_key(&k) {
            // Emulates ifGenerationMatch=0 returning 412 -> Ok(())
            return Ok(());
        }
        map.insert(k, data.to_vec());
        Ok(())
    }

    fn head_object(&self, bucket: &str, object_name: &str) -> io::Result<bool> {
        let map = self.objects.lock().unwrap();
        Ok(map.contains_key(&Self::key(bucket, object_name)))
    }

    fn delete_object(&self, bucket: &str, object_name: &str) -> io::Result<bool> {
        let mut map = self.objects.lock().unwrap();
        let k = Self::key(bucket, object_name);
        Ok(map.remove(&k).is_some())
    }
}

/// Content-Addressed Storage implementation backed by Google Cloud Storage.
pub struct GcsBlobStore {
    config: GcsConfig,
    transport: Arc<dyn GcsTransport>,
}

impl GcsBlobStore {
    /// Creates a new `GcsBlobStore` connected to a live GCS bucket.
    pub fn new(url_str: &str) -> io::Result<Self> {
        let config = GcsConfig::parse(url_str)?;
        let token_provider = Arc::new(GcsTokenProvider::new());
        let transport = Arc::new(UreqGcsTransport::new(&config.endpoint, token_provider));
        Ok(Self { config, transport })
    }

    /// Creates a new `GcsBlobStore` with a custom transport (e.g. for testing).
    pub fn with_transport(config: GcsConfig, transport: Arc<dyn GcsTransport>) -> Self {
        Self { config, transport }
    }

    /// Returns the configuration for this store.
    pub fn config(&self) -> &GcsConfig {
        &self.config
    }
}

impl BlobStore for GcsBlobStore {
    fn put(&self, data: &[u8]) -> io::Result<Digest> {
        let digest = Digest::from_bytes(data);
        let object_key = self.config.object_key_for_digest(&digest);
        self.transport
            .put_object_if_absent(&self.config.bucket, &object_key, data)?;
        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        let object_key = self.config.object_key_for_digest(digest);
        let maybe_bytes = self
            .transport
            .get_object(&self.config.bucket, &object_key)?;

        if let Some(bytes) = maybe_bytes {
            // Cryptographic bit-rot and integrity validation
            let computed = Digest::from_bytes(&bytes);
            if computed.hash != digest.hash {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "GCS corruption detected for {}: expected {}, computed {}",
                        digest, digest, computed
                    ),
                ));
            }
            if digest.size_bytes > 0 && computed.size_bytes != digest.size_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "GCS size mismatch for {}: expected {}, computed {}",
                        digest, digest.size_bytes, computed.size_bytes
                    ),
                ));
            }
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }

    fn has(&self, digest: &Digest) -> bool {
        let object_key = self.config.object_key_for_digest(digest);
        self.transport
            .head_object(&self.config.bucket, &object_key)
            .unwrap_or(false)
    }

    fn get_file_path(&self, _digest: &Digest) -> Option<PathBuf> {
        // Remote object stores do not have a local POSIX path.
        // Use CachedBlobStore to transparently hydrate blobs to local disk on-demand.
        None
    }

    fn delete(&self, digest: &Digest) -> io::Result<bool> {
        let object_key = self.config.object_key_for_digest(digest);
        self.transport
            .delete_object(&self.config.bucket, &object_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gcs_config_parsing() {
        let c1 = GcsConfig::parse("gs://my-bucket/cas/root").unwrap();
        assert_eq!(c1.bucket, "my-bucket");
        assert_eq!(c1.prefix, "cas/root");

        let c2 = GcsConfig::parse("gs://my-bucket/graft-test").unwrap();
        assert_eq!(c2.bucket, "my-bucket");
        assert_eq!(c2.prefix, "graft-test");

        let c3 = GcsConfig::parse("gs://simple-bucket").unwrap();
        assert_eq!(c3.bucket, "simple-bucket");
        assert_eq!(c3.prefix, "");

        assert!(GcsConfig::parse("https://storage.googleapis.com").is_err());
        assert!(GcsConfig::parse("gs://").is_err());
    }

    #[test]
    fn test_percent_encode() {
        assert_eq!(percent_encode("foo/bar/baz.txt"), "foo%2Fbar%2Fbaz.txt");
        assert_eq!(percent_encode("blobs/38/4307e4"), "blobs%2F38%2F4307e4");
        assert_eq!(percent_encode("normal_file-123.rs"), "normal_file-123.rs");
    }

    #[test]
    fn test_mock_gcs_blob_store() -> io::Result<()> {
        let config = GcsConfig::parse("gs://test-bucket/cas").unwrap();
        let transport = Arc::new(MockGcsTransport::new());
        let store = GcsBlobStore::with_transport(config, transport);

        let data = b"GCS Content-Addressed Storage Mock Test";
        let digest = store.put(data)?;

        assert!(store.has(&digest));

        let retrieved = store.get(&digest)?.expect("Blob must exist in mock GCS");
        assert_eq!(retrieved, data);

        // Putting identical data again should be idempotent
        let digest2 = store.put(data)?;
        assert_eq!(digest, digest2);

        // Missing digest returns None
        let missing = Digest::from_bytes(b"non-existent-payload");
        assert!(!store.has(&missing));
        assert_eq!(store.get(&missing)?, None);

        // Delete blob
        assert!(store.delete(&digest)?);
        assert!(!store.has(&digest));
        assert!(!store.delete(&digest)?);

        Ok(())
    }
}
