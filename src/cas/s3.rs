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

//! # S3-Compatible Remote Content-Addressed Storage (CAS)
//!
//! Provides a production-grade Content-Addressed Storage implementation backed by
//! Amazon S3, Cloudflare R2 ($0 egress), MinIO, Ceph, and LocalStack (`s3://<bucket>/<prefix>`).
//!
//! ### Key Capabilities:
//! 1. **Multi-Cloud Portability**: Seamlessly targets AWS S3, Cloudflare R2, MinIO, or custom
//!    S3-compliant gateways using standard `s3://` URLs.
//! 2. **Embedded AWS SigV4 Authentication**: Built-in, zero-dependency AWS Signature Version 4
//!    signing using `ring` (HMAC-SHA256), requiring no external AWS SDK dependencies.
//! 3. **Bit-Rot & Transfer Integrity**: Validates every downloaded payload against its expected
//!    BLAKE3 digest and byte length before surfacing to the virtual filesystem or cache.
//! 4. **Pluggable Transport**: Includes [`UreqS3Transport`] for live network access and
//!    [`MockS3Transport`] for hermetic, high-speed unit testing.

use crate::cas::blob_store::BlobStore;
use crate::cas::digest::Digest;
use std::collections::HashMap;
use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Configuration for S3-compatible bucket, endpoint, and credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Config {
    /// S3 Bucket name (e.g. "my-graft-cas")
    pub bucket: String,
    /// Key prefix inside bucket (e.g. "cas" or "graft-data")
    pub prefix: String,
    /// Base HTTP/HTTPS endpoint (e.g. "https://s3.amazonaws.com", "http://localhost:9000")
    pub endpoint: String,
    /// AWS / S3 region (default: "us-east-1" or "auto" for Cloudflare R2)
    pub region: String,
    /// Whether to force path-style URLs (`endpoint/bucket/key`) rather than virtual host style
    pub force_path_style: bool,
    /// AWS Access Key ID
    pub access_key_id: Option<String>,
    /// AWS Secret Access Key
    pub secret_access_key: Option<String>,
    /// Optional AWS Session Token
    pub session_token: Option<String>,
}

impl S3Config {
    /// Parses an `s3://<bucket>[/<prefix>]` URL string, populating endpoint and credentials
    /// from standard environment variables (`AWS_ENDPOINT_URL`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, etc.).
    pub fn parse(url_str: &str) -> io::Result<Self> {
        let trimmed = url_str.trim();
        if !trimmed.starts_with("s3://") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid S3 URL '{}': must start with 's3://'", url_str),
            ));
        }

        let without_scheme = &trimmed["s3://".len()..];
        let mut parts = without_scheme.splitn(2, '/');
        let bucket = parts.next().unwrap_or("").trim().to_string();
        if bucket.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Invalid S3 URL '{}': missing bucket name", url_str),
            ));
        }

        let raw_prefix = parts.next().unwrap_or("").trim();
        let prefix = raw_prefix
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();

        let endpoint = env::var("GRAFT_S3_ENDPOINT")
            .or_else(|_| env::var("AWS_ENDPOINT_URL_S3"))
            .or_else(|_| env::var("AWS_ENDPOINT_URL"))
            .unwrap_or_else(|_| "https://s3.amazonaws.com".to_string());

        let region = env::var("GRAFT_S3_REGION")
            .or_else(|_| env::var("AWS_REGION"))
            .or_else(|_| env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());

        let force_path_style = env::var("GRAFT_S3_FORCE_PATH_STYLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| {
                // Auto-detect MinIO / Localhost / IP endpoints
                endpoint.contains("localhost")
                    || endpoint.contains("127.0.0.1")
                    || endpoint.contains(":9000")
                    || endpoint.contains("r2.cloudflarestorage.com")
            });

        let access_key_id = env::var("GRAFT_S3_ACCESS_KEY")
            .or_else(|_| env::var("AWS_ACCESS_KEY_ID"))
            .ok();
        let secret_access_key = env::var("GRAFT_S3_SECRET_KEY")
            .or_else(|_| env::var("AWS_SECRET_ACCESS_KEY"))
            .ok();
        let session_token = env::var("AWS_SESSION_TOKEN").ok();

        Ok(Self {
            bucket,
            prefix,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
            session_token,
        })
    }

    /// Computes the object storage key inside the bucket for a given digest.
    pub fn object_key_for_digest(&self, digest: &Digest) -> String {
        let shard_prefix = digest.shard_prefix();
        let shard_suffix = digest.shard_suffix();
        if self.prefix.is_empty() {
            format!("blobs/{}/{}", shard_prefix, shard_suffix)
        } else {
            format!("{}/blobs/{}/{}", self.prefix, shard_prefix, shard_suffix)
        }
    }

    /// Computes the full HTTP URL for an object key.
    pub fn object_url(&self, object_key: &str) -> String {
        let clean_key = object_key.trim_start_matches('/');
        if self.force_path_style || self.endpoint.contains("localhost") || self.endpoint.contains("127.0.0.1") {
            format!("{}/{}/{}", self.endpoint, self.bucket, clean_key)
        } else if self.endpoint == "https://s3.amazonaws.com" {
            format!("https://{}.s3.{}.amazonaws.com/{}", self.bucket, self.region, clean_key)
        } else {
            format!("{}/{}/{}", self.endpoint, self.bucket, clean_key)
        }
    }
}

/// Helper function to perform RFC 3986 percent-encoding for URI paths.
pub fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// Computes UTC ISO 8601 timestamps required by AWS SigV4 (`(amz_date, date_stamp)`).
pub fn format_amz_date(epoch_seconds: u64) -> (String, String) {
    let days = (epoch_seconds / 86400) as i64;
    let rem_secs = (epoch_seconds % 86400) as u32;
    let hours = rem_secs / 3600;
    let minutes = (rem_secs % 3600) / 60;
    let seconds = rem_secs % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142401) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let date_stamp = format!("{:04}{:02}{:02}", y, m, d);
    let amz_date = format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", y, m, d, hours, minutes, seconds);
    (amz_date, date_stamp)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let s_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let tag = ring::hmac::sign(&s_key, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    hex::encode(digest.as_ref())
}

/// Signs an HTTP request using standard AWS Signature Version 4.
pub fn sign_v4(
    method: &str,
    url_str: &str,
    headers: &mut Vec<(String, String)>,
    body: &[u8],
    region: &str,
    service: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
    now_epoch: u64,
) -> io::Result<()> {
    let url = url::Url::parse(url_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("Invalid URL: {}", e)))?;

    let host = match url.port() {
        Some(port) => format!("{}:{}", url.host_str().unwrap_or(""), port),
        None => url.host_str().unwrap_or("").to_string(),
    };

    let (amz_date, date_stamp) = format_amz_date(now_epoch);
    let payload_hash = sha256_hex(body);

    // Add necessary AWS headers
    headers.push(("host".to_string(), host));
    headers.push(("x-amz-date".to_string(), amz_date.clone()));
    headers.push(("x-amz-content-sha256".to_string(), payload_hash.clone()));

    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".to_string(), token.to_string()));
    }

    // Sort headers for canonical request
    headers.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let mut canonical_headers = String::new();
    let mut signed_headers_vec = Vec::new();
    for (k, v) in headers.iter() {
        let lower_k = k.to_lowercase();
        canonical_headers.push_str(&format!("{}:{}\n", lower_k, v.trim()));
        signed_headers_vec.push(lower_k);
    }
    let signed_headers = signed_headers_vec.join(";");

    let canonical_uri = percent_encode_path(url.path());
    let canonical_query = url.query().unwrap_or("");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date, credential_scope, canonical_request_hash
    );

    // Derive signing key
    let k_secret = format!("AWS4{}", secret_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");

    // Compute signature
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    );

    headers.push(("authorization".to_string(), auth_header));
    Ok(())
}

/// Abstract transport trait for S3 HTTP/HTTPS operations.
pub trait S3Transport: Send + Sync {
    /// Fetches an object from S3. Returns `Ok(None)` if 404 Not Found.
    fn get_object(&self, bucket: &str, object_key: &str) -> io::Result<Option<Vec<u8>>>;

    /// Uploads an object to S3 if not already present.
    fn put_object_if_absent(&self, bucket: &str, object_key: &str, data: &[u8]) -> io::Result<()>;

    /// Checks whether an object exists in S3 (via HEAD).
    fn head_object(&self, bucket: &str, object_key: &str) -> io::Result<bool>;

    /// Deletes an object from S3.
    fn delete_object(&self, bucket: &str, object_key: &str) -> io::Result<bool>;
}

/// Live HTTP/HTTPS transport using `ureq` and AWS SigV4 signing.
pub struct UreqS3Transport {
    config: S3Config,
}

impl UreqS3Transport {
    /// Creates a live S3 transport for the specified configuration.
    pub fn new(config: S3Config) -> Self {
        Self { config }
    }

    fn prepare_headers(&self, method: &str, url: &str, body: &[u8]) -> io::Result<Vec<(String, String)>> {
        let mut headers = Vec::new();
        if let (Some(ak), Some(sk)) = (&self.config.access_key_id, &self.config.secret_access_key) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            sign_v4(
                method,
                url,
                &mut headers,
                body,
                &self.config.region,
                "s3",
                ak,
                sk,
                self.config.session_token.as_deref(),
                now,
            )?;
        }
        Ok(headers)
    }
}

impl S3Transport for UreqS3Transport {
    fn get_object(&self, _bucket: &str, object_key: &str) -> io::Result<Option<Vec<u8>>> {
        let url = self.config.object_url(object_key);
        let headers = self.prepare_headers("GET", &url, &[])?;

        let mut req = ureq::get(&url);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        match req.call() {
            Ok(mut resp) => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut resp.body_mut().as_reader(), &mut bytes)?;
                Ok(Some(bytes))
            }
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("S3 GET for object '{}' failed: {}", object_key, e),
            )),
        }
    }

    fn put_object_if_absent(&self, bucket: &str, object_key: &str, data: &[u8]) -> io::Result<()> {
        if self.head_object(bucket, object_key)? {
            return Ok(());
        }

        let url = self.config.object_url(object_key);
        let mut headers = self.prepare_headers("PUT", &url, data)?;
        headers.push(("If-None-Match".to_string(), "*".to_string()));

        let mut req = ureq::put(&url);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        match req.send(data) {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(412)) => Ok(()), // Precondition failed: already exists
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("S3 PUT for object '{}' failed: {}", object_key, e),
            )),
        }
    }

    fn head_object(&self, _bucket: &str, object_key: &str) -> io::Result<bool> {
        let url = self.config.object_url(object_key);
        let headers = self.prepare_headers("HEAD", &url, &[])?;

        let mut req = ureq::head(&url);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        match req.call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("S3 HEAD for object '{}' failed: {}", object_key, e),
            )),
        }
    }

    fn delete_object(&self, _bucket: &str, object_key: &str) -> io::Result<bool> {
        let url = self.config.object_url(object_key);
        let headers = self.prepare_headers("DELETE", &url, &[])?;

        let mut req = ureq::delete(&url);
        for (k, v) in headers {
            req = req.header(&k, &v);
        }

        match req.call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("S3 DELETE for object '{}' failed: {}", object_key, e),
            )),
        }
    }
}

/// In-memory mock transport for hermetic offline testing.
#[derive(Debug, Default)]
pub struct MockS3Transport {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MockS3Transport {
    /// Creates an empty in-memory mock S3 transport.
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn key(bucket: &str, object_key: &str) -> String {
        format!("{}/{}", bucket, object_key)
    }
}

impl S3Transport for MockS3Transport {
    fn get_object(&self, bucket: &str, object_key: &str) -> io::Result<Option<Vec<u8>>> {
        let map = self.objects.lock().unwrap();
        Ok(map.get(&Self::key(bucket, object_key)).cloned())
    }

    fn put_object_if_absent(&self, bucket: &str, object_key: &str, data: &[u8]) -> io::Result<()> {
        let mut map = self.objects.lock().unwrap();
        let k = Self::key(bucket, object_key);
        if map.contains_key(&k) {
            return Ok(());
        }
        map.insert(k, data.to_vec());
        Ok(())
    }

    fn head_object(&self, bucket: &str, object_key: &str) -> io::Result<bool> {
        let map = self.objects.lock().unwrap();
        Ok(map.contains_key(&Self::key(bucket, object_key)))
    }

    fn delete_object(&self, bucket: &str, object_key: &str) -> io::Result<bool> {
        let mut map = self.objects.lock().unwrap();
        Ok(map.remove(&Self::key(bucket, object_key)).is_some())
    }
}

/// Content-Addressed Storage implementation backed by S3-compatible object storage.
pub struct S3BlobStore {
    config: S3Config,
    transport: Arc<dyn S3Transport>,
}

impl S3BlobStore {
    /// Creates a new `S3BlobStore` from an `s3://` URL.
    pub fn new(url_str: &str) -> io::Result<Self> {
        let config = S3Config::parse(url_str)?;
        let transport = Arc::new(UreqS3Transport::new(config.clone()));
        Ok(Self { config, transport })
    }

    /// Creates an `S3BlobStore` with a custom transport (e.g. `MockS3Transport`).
    pub fn with_transport(config: S3Config, transport: Arc<dyn S3Transport>) -> Self {
        Self { config, transport }
    }

    /// Returns the active configuration.
    pub fn config(&self) -> &S3Config {
        &self.config
    }
}

impl BlobStore for S3BlobStore {
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
                        "S3 corruption detected for {}: expected {}, computed {}",
                        digest, digest, computed
                    ),
                ));
            }
            if digest.size_bytes > 0 && computed.size_bytes != digest.size_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "S3 size mismatch for {}: expected {} bytes, computed {} bytes",
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
        // Remote object stores do not expose a local POSIX file path.
        // CachedBlobStore transparently hydrates blobs to local NVMe SSD on-demand.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_s3_config_parsing() {
        let cfg = S3Config::parse("s3://my-bucket/cas-data").unwrap();
        assert_eq!(cfg.bucket, "my-bucket");
        assert_eq!(cfg.prefix, "cas-data");

        let root_cfg = S3Config::parse("s3://root-bucket").unwrap();
        assert_eq!(root_cfg.bucket, "root-bucket");
        assert_eq!(root_cfg.prefix, "");

        let invalid = S3Config::parse("gs://wrong-scheme");
        assert!(invalid.is_err());
    }

    #[test]
    fn test_s3_object_key_generation() {
        let cfg = S3Config::parse("s3://my-bucket/cas").unwrap();
        let digest = Digest::parse("38a0c20000000000000000000000000000000000000000000000000000000000:10").unwrap();
        assert_eq!(
            cfg.object_key_for_digest(&digest),
            "cas/blobs/38/a0c20000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn test_amz_date_formatting() {
        // 2026-11-05T03:30:00Z -> epoch 1793849400
        let (amz_date, date_stamp) = format_amz_date(1793849400);
        assert_eq!(date_stamp, "20261105");
        assert_eq!(amz_date, "20261105T033000Z");
    }

    #[test]
    fn test_sigv4_signing() {
        let mut headers = Vec::new();
        let body = b"hello graft s3";
        let res = sign_v4(
            "PUT",
            "https://my-bucket.s3.us-east-1.amazonaws.com/blobs/test",
            &mut headers,
            body,
            "us-east-1",
            "s3",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            None,
            1793849400,
        );
        assert!(res.is_ok());

        let auth_hdr = headers.iter().find(|(k, _)| k == "authorization").unwrap();
        assert!(auth_hdr.1.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20261105/us-east-1/s3/aws4_request"));
        assert!(auth_hdr.1.contains("Signature="));
    }

    #[test]
    fn test_s3_blob_store_mock_lifecycle() {
        let cfg = S3Config::parse("s3://test-bucket/cas").unwrap();
        let transport = Arc::new(MockS3Transport::new());
        let store = S3BlobStore::with_transport(cfg, transport);

        let data = b"content-addressed blob data for AI agents";
        let digest = store.put(data).unwrap();
        assert_eq!(digest.size_bytes, data.len());

        // Existence check
        assert!(store.has(&digest));

        // Retrieval check
        let fetched = store.get(&digest).unwrap();
        assert_eq!(fetched, Some(data.to_vec()));

        // Non-existent check
        let fake = Digest::parse("0000000000000000000000000000000000000000000000000000000000000000:5").unwrap();
        assert!(!store.has(&fake));
        assert_eq!(store.get(&fake).unwrap(), None);
    }

    #[test]
    fn test_s3_blob_store_corruption_detection() {
        let cfg = S3Config::parse("s3://test-bucket/cas").unwrap();
        let transport = Arc::new(MockS3Transport::new());
        let store = S3BlobStore::with_transport(cfg.clone(), transport.clone());

        let data = b"valid uncorrupted payload";
        let digest = store.put(data).unwrap();

        // Tamper with underlying object in mock storage
        let object_key = cfg.object_key_for_digest(&digest);
        let mut tampered = data.to_vec();
        tampered[0] ^= 0xFF; // Bit flip
        transport
            .objects
            .lock()
            .unwrap()
            .insert(MockS3Transport::key(&cfg.bucket, &object_key), tampered);

        // Fetching should detect cryptographic corruption
        let err = store.get(&digest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("S3 corruption detected"));
    }
}
