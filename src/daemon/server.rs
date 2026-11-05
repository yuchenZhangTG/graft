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

//! # graftd HTTP/1.1 Asynchronous Server
//!
//! Tokio-powered async HTTP server featuring Keep-Alive connection reuse,
//! multi-packet `read_exact` body handling, 100-continue handshakes,
//! and `spawn_blocking` store offloading.

use crate::cas::blob_store::BlobStore;
use crate::daemon::{cas_service, lease_service, merge_service, ref_service};
use crate::metadata::MetadataStore;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

pub struct DaemonState {
    pub blob_store: Arc<dyn BlobStore>,
    pub meta_store: Arc<dyn MetadataStore>,
    pub auth_token: Option<String>,
}

/// Runs the daemon server loop with a pre-bound listener.
/// Ideal for both production service and zero-race unit testing with `127.0.0.1:0`.
pub async fn serve_with_listener(
    listener: TcpListener,
    state: Arc<DaemonState>,
    mut shutdown_rx: Option<oneshot::Receiver<()>>,
) -> io::Result<()> {
    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, state_clone).await {
                                tracing::debug!("Connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Accept failed: {}", e);
                    }
                }
            }
            _ = async {
                if let Some(ref mut rx) = shutdown_rx {
                    let _ = rx.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::info!("Shutdown signal received, stopping graftd listener");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_connection(mut stream: TcpStream, state: Arc<DaemonState>) -> io::Result<()> {
    let mut buf = vec![0u8; 16384];

    loop {
        // 1. Read HTTP Request Headers up to \r\n\r\n
        let mut header_bytes = Vec::new();
        let mut end_of_headers = None;

        while header_bytes.len() < 65536 {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(()); // Client disconnected
            }
            header_bytes.extend_from_slice(&buf[..n]);

            if let Some(pos) = find_subsequence(&header_bytes, b"\r\n\r\n") {
                end_of_headers = Some(pos);
                break;
            }
        }

        let header_end = match end_of_headers {
            Some(pos) => pos,
            None => {
                // Header too large or malformed
                let _ = stream.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n").await;
                return Ok(());
            }
        };

        let header_str = String::from_utf8_lossy(&header_bytes[..header_end]);
        let mut lines = header_str.lines();
        let request_line = match lines.next() {
            Some(l) => l,
            None => return Ok(()),
        };

        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("GET").to_uppercase();
        let uri = parts.next().unwrap_or("/");

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }

        // 2. Expect: 100-continue handshake
        if headers.get("expect").map(|v| v.eq_ignore_ascii_case("100-continue")).unwrap_or(false) {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
            stream.flush().await?;
        }

        // 3. Read Body with exact Content-Length
        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if content_length > 104_857_600 {
            // Cap at 100 MB body
            let _ = stream.write_all(b"HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\n").await;
            return Ok(());
        }

        let body_already_read = &header_bytes[header_end + 4..];
        let mut body = Vec::with_capacity(content_length);
        let needed_from_existing = body_already_read.len().min(content_length);
        body.extend_from_slice(&body_already_read[..needed_from_existing]);

        let remaining = content_length.saturating_sub(body.len());
        if remaining > 0 {
            let mut remaining_buf = vec![0u8; remaining];
            stream.read_exact(&mut remaining_buf).await?;
            body.extend_from_slice(&remaining_buf);
        }

        // 4. Authenticate Request
        if let Some(ref required_token) = state.auth_token {
            let auth_header = headers.get("authorization").cloned().unwrap_or_default();
            let token_provided = if let Some(stripped) = auth_header.strip_prefix("Bearer ") {
                stripped.trim()
            } else {
                auth_header.trim()
            };
            if token_provided != required_token {
                let resp = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 26\r\n\r\n{\"error\":\"Unauthorized\"}";
                stream.write_all(resp).await?;
                continue;
            }
        }

        // 5. Dispatch Request (Offloaded to worker thread)
        let state_for_task = state.clone();
        let method_for_task = method.clone();
        let uri_for_task = uri.to_string();
        let body_for_task = body;

        let (status, content_type, response_body) = tokio::task::spawn_blocking(move || {
            route_request(&state_for_task, &method_for_task, &uri_for_task, &body_for_task)
        })
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // 6. Write HTTP Response
        let status_text = match status {
            200 => "200 OK",
            201 => "201 Created",
            400 => "400 Bad Request",
            401 => "401 Unauthorized",
            403 => "403 Forbidden",
            404 => "404 Not Found",
            409 => "409 Conflict",
            413 => "413 Payload Too Large",
            _ => "500 Internal Server Error",
        };

        let is_keep_alive = headers
            .get("connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);

        let conn_header = if is_keep_alive { "keep-alive" } else { "close" };

        let header_str = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\nServer: graftd/0.2.0\r\n\r\n",
            status_text,
            content_type,
            response_body.len(),
            conn_header
        );

        stream.write_all(header_str.as_bytes()).await?;
        if method != "HEAD" {
            stream.write_all(&response_body).await?;
        }
        stream.flush().await?;

        if !is_keep_alive {
            break;
        }
    }

    Ok(())
}

fn route_request(
    state: &DaemonState,
    method: &str,
    uri: &str,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let clean_path = uri.split('?').next().unwrap_or(uri);

    match (method, clean_path) {
        // Health & Status
        ("GET", "/healthz") => (200, "application/json", br#"{"status":"healthy"}"#.to_vec()),
        ("GET", "/api/v1/status") => (
            200,
            "application/json",
            br#"{"status":"ok","daemon":"graftd","version":"0.2.0"}"#.to_vec(),
        ),

        // CAS Endpoints
        ("HEAD", p) if p.starts_with("/api/v1/cas/blobs/") => {
            let digest_str = &p["/api/v1/cas/blobs/".len()..];
            cas_service::handle_has_blob(&state.blob_store, digest_str)
        }
        ("GET", p) if p.starts_with("/api/v1/cas/blobs/") => {
            let digest_str = &p["/api/v1/cas/blobs/".len()..];
            cas_service::handle_get_blob(&state.blob_store, digest_str)
        }
        ("PUT", p) if p.starts_with("/api/v1/cas/blobs/") => {
            let digest_str = &p["/api/v1/cas/blobs/".len()..];
            cas_service::handle_put_blob(&state.blob_store, Some(digest_str), body)
        }
        ("POST", "/api/v1/cas/blobs") => {
            cas_service::handle_put_blob(&state.blob_store, None, body)
        }
        ("POST", "/api/v1/cas/missing") => {
            cas_service::handle_find_missing(&state.blob_store, body)
        }

        // Reference Endpoints
        ("POST", "/api/v1/refs/cas") => ref_service::handle_cas_ref(&state.meta_store, body),
        ("GET", "/api/v1/refs") => {
            let query = uri.split('?').nth(1).unwrap_or("");
            let prefix = extract_query_param(query, "prefix").unwrap_or_default();
            ref_service::handle_list_refs(&state.meta_store, &prefix)
        }
        ("GET", p) if p.starts_with("/api/v1/refs/") => {
            let ref_name = crate::web::url_decode(&p["/api/v1/refs/".len()..]);
            ref_service::handle_get_ref(&state.meta_store, &ref_name)
        }
        ("DELETE", p) if p.starts_with("/api/v1/refs/") => {
            let ref_name = crate::web::url_decode(&p["/api/v1/refs/".len()..]);
            let query = uri.split('?').nth(1).unwrap_or("");
            let expected_query = extract_query_param(query, "expected");
            ref_service::handle_delete_ref(&state.meta_store, &ref_name, expected_query.as_deref(), body)
        }

        // Commit Endpoints
        ("GET", p) if p.starts_with("/api/v1/commits/") => {
            let commit_digest = &p["/api/v1/commits/".len()..];
            ref_service::handle_get_commit(&state.meta_store, commit_digest)
        }
        ("POST", "/api/v1/commits") => ref_service::handle_put_commit(&state.meta_store, body),

        // Workspace Lease Endpoints
        ("POST", "/api/v1/leases/acquire") => lease_service::handle_acquire_lease(&state.meta_store, body),
        ("POST", "/api/v1/leases/renew") => lease_service::handle_renew_lease(&state.meta_store, body),
        ("POST", "/api/v1/leases/release") => lease_service::handle_release_lease(&state.meta_store, body),
        ("GET", "/api/v1/leases") => lease_service::handle_list_leases(&state.meta_store),
        ("GET", p) if p.starts_with("/api/v1/leases/") => {
            let ws_id = &p["/api/v1/leases/".len()..];
            lease_service::handle_get_lease(&state.meta_store, ws_id)
        }

        // Server-Side AST Semantic Merge
        ("POST", "/api/v1/merge") => {
            merge_service::handle_server_side_merge(&state.blob_store, &state.meta_store, body)
        }

        _ => (404, "application/json", br#"{"error":"Endpoint not found"}"#.to_vec()),
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key {
                return Some(crate::web::url_decode(v));
            }
        }
    }
    None
}
