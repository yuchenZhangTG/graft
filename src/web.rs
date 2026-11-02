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

//! # Graft Web Subsystem
//!
//! Embedded high-performance Web server for code search, repository browsing,
//! diff inspection, and active AI agent workspace monitoring.

pub mod handlers;
pub mod markdown;
pub mod search;
pub mod templates;

pub use handlers::*;
pub use markdown::*;
pub use search::*;
pub use templates::*;

use crate::cas::blob_store::BlobStore;
use crate::metadata::MetadataStore;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Configuration for launching the Graft Web UI.
pub struct WebConfig {
    pub host: String,
    pub port: u16,
    pub default_branch: String,
    pub storage_dir: PathBuf,
}

/// Simple URL decoding helper.
pub fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(val);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Parses a query string ("key=val&other=123") into a key-value HashMap.
pub fn parse_query_params(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            params.insert(url_decode(k), url_decode(v));
        } else if !pair.is_empty() {
            params.insert(url_decode(pair), String::new());
        }
    }
    params
}

/// Helper to parse request body into key-value map (supporting urlencoded form or JSON).
pub fn parse_body_params(body: &[u8]) -> HashMap<String, String> {
    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(obj) = json_val.as_object() {
            let mut map = HashMap::new();
            for (k, v) in obj {
                match v {
                    serde_json::Value::String(s) => {
                        map.insert(k.clone(), s.clone());
                    }
                    serde_json::Value::Number(n) => {
                        map.insert(k.clone(), n.to_string());
                    }
                    serde_json::Value::Bool(b) => {
                        map.insert(k.clone(), b.to_string());
                    }
                    _ => {
                        map.insert(k.clone(), v.to_string());
                    }
                }
            }
            return map;
        }
    }
    // Fall back to urlencoded form params
    let body_str = String::from_utf8_lossy(body);
    parse_query_params(&body_str)
}

/// Dispatches an incoming HTTP request (with method and body) to the appropriate handler.
pub fn route_request_full(
    method: &str,
    uri: &str,
    body: &[u8],
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    storage_dir: &std::path::Path,
    default_branch: &str,
) -> HttpResponse {
    let (path_part, query_part) = uri.split_once('?').unwrap_or((uri, ""));
    let query_params = parse_query_params(query_part);
    let path = url_decode(path_part);

    // POST Request Dispatch
    if method == "POST" {
        let body_params = parse_body_params(body);

        if path == "/api/pulls/new" {
            let is_api = query_params.get("format").map(|s| s == "json").unwrap_or(false)
                || body_params.get("format").map(|s| s == "json").unwrap_or(false);
            return handle_create_pr_post(&body_params, storage_dir, blob_store, meta_store, is_api);
        }

        if path == "/api/pulls/comment" {
            let is_api = query_params.get("format").map(|s| s == "json").unwrap_or(false)
                || body_params.get("format").map(|s| s == "json").unwrap_or(false);
            return handle_pr_comment_post(&body_params, storage_dir, is_api);
        }

        if path == "/api/pulls/merge" {
            let is_api = query_params.get("format").map(|s| s == "json").unwrap_or(false)
                || body_params.get("format").map(|s| s == "json").unwrap_or(false);
            return handle_pr_merge_post(&body_params, storage_dir, blob_store, meta_store, is_api);
        }

        return not_found_response(&format!("POST route '{}' not found", path));
    }

    // Root redirect or home page
    if path == "/" || path.is_empty() {
        return (
            302,
            "text/html; charset=utf-8",
            format!(
                "Location: /tree/{}/\r\n",
                default_branch
            )
            .into_bytes(),
        );
    }

    // REST API Endpoints
    if path == "/api/info" {
        let branches = meta_store.list_refs("refs/heads/").unwrap_or_default();
        let branch_json: Vec<_> = branches
            .into_iter()
            .map(|(r, d)| json!({ "ref": r, "commit": d.to_string() }))
            .collect();
        return json_response(json!({
            "service": "Graft Web",
            "version": "0.1.0",
            "default_branch": default_branch,
            "branches": branch_json,
        }));
    }

    if path == "/api/tree" {
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let tree_path = query_params.get("path").map(|s| s.as_str()).unwrap_or("");
        return handle_tree(ref_name, tree_path, blob_store, meta_store, true);
    }

    if path == "/api/blob" {
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let blob_path = query_params.get("path").map(|s| s.as_str()).unwrap_or("");
        return handle_blob(ref_name, blob_path, blob_store, meta_store, false, true);
    }

    if path == "/api/blame" {
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let blob_path = query_params.get("path").map(|s| s.as_str()).unwrap_or("");
        return handle_blame(ref_name, blob_path, blob_store, meta_store, true);
    }

    if path == "/api/search" {
        let q = query_params.get("q").map(|s| s.as_str()).unwrap_or("");
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let path_filter = query_params.get("path").map(|s| s.as_str()).filter(|s| !s.is_empty());
        let engine = query_params.get("engine").map(|s| s.as_str()).unwrap_or("level2");
        let is_regex = query_params.get("regex").map(|s| s == "1" || s == "true").unwrap_or(false);
        let is_case = query_params.get("case").map(|s| s == "1" || s == "true").unwrap_or(false);
        return handle_search(q, ref_name, path_filter, engine, is_regex, is_case, blob_store, meta_store, true);
    }

    if path == "/api/commits" {
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let limit = query_params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(50);
        return handle_commits(ref_name, limit, blob_store, meta_store, true);
    }

    if path == "/api/diff" {
        let commit = query_params.get("commit").map(|s| s.as_str()).unwrap_or(default_branch);
        let base = query_params.get("base").map(|s| s.as_str());
        let target = query_params.get("target").map(|s| s.as_str());
        if let (Some(b), Some(t)) = (base, target) {
            return handle_diff(b, Some(t), blob_store, meta_store, true);
        } else {
            return handle_diff(commit, None, blob_store, meta_store, true);
        }
    }

    if path == "/api/pulls" {
        return handle_pulls(storage_dir, blob_store, meta_store, default_branch, true);
    }

    if let Some(rest) = path.strip_prefix("/api/pulls/") {
        if let Ok(id) = rest.parse::<usize>() {
            return handle_pull_detail(id, "conversation", storage_dir, blob_store, meta_store, default_branch, true);
        }
    }

    if path == "/api/workspaces" {
        return handle_workspaces(storage_dir, blob_store, true);
    }

    if let Some(rest) = path.strip_prefix("/api/workspaces/") {
        if let Some(id) = rest.strip_suffix("/diff") {
            return handle_workspace_diff(id, storage_dir, blob_store, true);
        }
    }

    // HTML Views
    if let Some(rest) = path.strip_prefix("/tree/") {
        let (ref_name, subpath) = if let Some((r, s)) = rest.split_once('/') {
            (r, s)
        } else {
            (rest, "")
        };
        let active_ref = if ref_name.is_empty() { default_branch } else { ref_name };
        return handle_tree(active_ref, subpath, blob_store, meta_store, false);
    }

    if let Some(rest) = path.strip_prefix("/blob/") {
        let (ref_name, subpath) = if let Some((r, s)) = rest.split_once('/') {
            (r, s)
        } else {
            (rest, "")
        };
        let active_ref = if ref_name.is_empty() { default_branch } else { ref_name };
        return handle_blob(active_ref, subpath, blob_store, meta_store, false, false);
    }

    if let Some(rest) = path.strip_prefix("/blame/") {
        let (ref_name, subpath) = if let Some((r, s)) = rest.split_once('/') {
            (r, s)
        } else {
            (rest, "")
        };
        let active_ref = if ref_name.is_empty() { default_branch } else { ref_name };
        return handle_blame(active_ref, subpath, blob_store, meta_store, false);
    }

    if let Some(rest) = path.strip_prefix("/raw/") {
        let (ref_name, subpath) = if let Some((r, s)) = rest.split_once('/') {
            (r, s)
        } else {
            (rest, "")
        };
        let active_ref = if ref_name.is_empty() { default_branch } else { ref_name };
        return handle_blob(active_ref, subpath, blob_store, meta_store, true, false);
    }

    if path == "/search" {
        let q = query_params.get("q").map(|s| s.as_str()).unwrap_or("");
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let path_filter = query_params.get("path").map(|s| s.as_str()).filter(|s| !s.is_empty());
        let engine = query_params.get("engine").map(|s| s.as_str()).unwrap_or("level2");
        let is_regex = query_params.get("regex").map(|s| s == "1" || s == "true").unwrap_or(false);
        let is_case = query_params.get("case").map(|s| s == "1" || s == "true").unwrap_or(false);
        return handle_search(q, ref_name, path_filter, engine, is_regex, is_case, blob_store, meta_store, false);
    }

    if path == "/commits" {
        let ref_name = query_params.get("ref").map(|s| s.as_str()).unwrap_or(default_branch);
        let limit = query_params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(50);
        return handle_commits(ref_name, limit, blob_store, meta_store, false);
    }

    if let Some(commit) = path.strip_prefix("/diff/") {
        return handle_diff(commit, None, blob_store, meta_store, false);
    }

    if path == "/pulls" {
        return handle_pulls(storage_dir, blob_store, meta_store, default_branch, false);
    }

    if path == "/pulls/new" {
        return handle_new_pr_page(meta_store, default_branch);
    }

    if let Some(rest) = path.strip_prefix("/pull/") {
        if let Ok(id) = rest.parse::<usize>() {
            let subtab = query_params.get("tab").map(|s| s.as_str()).unwrap_or("conversation");
            return handle_pull_detail(id, subtab, storage_dir, blob_store, meta_store, default_branch, false);
        }
    }

    if path == "/workspaces" {
        return handle_workspaces(storage_dir, blob_store, false);
    }

    if let Some(rest) = path.strip_prefix("/workspaces/") {
        if let Some(id) = rest.strip_suffix("/diff") {
            return handle_workspace_diff(id, storage_dir, blob_store, false);
        }
    }

    not_found_response(&format!("Path '{}' does not match any route", path))
}

/// Backward-compatible route_request for GET requests.
pub fn route_request(
    uri: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    storage_dir: &std::path::Path,
    default_branch: &str,
) -> HttpResponse {
    route_request_full("GET", uri, b"", blob_store, meta_store, storage_dir, default_branch)
}

/// Launches the embedded async Graft Web server.
pub async fn start_web_server(
    config: WebConfig,
    blob_store: Arc<dyn BlobStore>,
    meta_store: Arc<dyn MetadataStore>,
) -> anyhow::Result<()> {
    let bind_addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind_addr).await?;

    println!("╔════════════════════════════════════════════════════════════════════╗");
    println!("║                   🌱 GRAFT WEB DASHBOARD                           ║");
    println!("╠════════════════════════════════════════════════════════════════════╣");
    println!("║  Local URL:    http://localhost:{}                              ║", config.port);
    println!("║  Bind Addr:    http://{:<45} ║", bind_addr);
    println!("║  Default Ref:  {:<52} ║", config.default_branch);
    println!("║  Storage Dir:  {:<52} ║", config.storage_dir.display());
    println!("╚════════════════════════════════════════════════════════════════════╝");
    println!("Ready for Code Search, Tree Browsing & Workspace Monitoring. Press Ctrl-C to stop.");

    let default_branch = Arc::new(config.default_branch);
    let storage_dir = Arc::new(config.storage_dir);

    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to accept incoming TCP connection: {}", e);
                continue;
            }
        };

        let blob_store = Arc::clone(&blob_store);
        let meta_store = Arc::clone(&meta_store);
        let default_branch = Arc::clone(&default_branch);
        let storage_dir = Arc::clone(&storage_dir);

        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            let n = match socket.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };

            let req_str = String::from_utf8_lossy(&buf[..n]);
            let first_line = req_str.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();

            if parts.len() < 2 || (parts[0] != "GET" && parts[0] != "POST") {
                let _ = socket.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n").await;
                return;
            }

            let method = parts[0];
            let uri = parts[1];

            // Extract body bytes for POST
            let body_bytes = if method == "POST" {
                let raw = &buf[..n];
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    &raw[pos + 4..]
                } else if let Some(pos) = raw.windows(2).position(|w| w == b"\n\n") {
                    &raw[pos + 2..]
                } else {
                    b""
                }
            } else {
                b""
            };

            let (status, content_type, body) = route_request_full(
                method,
                uri,
                body_bytes,
                blob_store.as_ref(),
                meta_store.as_ref(),
                &storage_dir,
                &default_branch,
            );

            let status_msg = match status {
                200 => "OK",
                302 => "Found",
                404 => "Not Found",
                405 => "Method Not Allowed",
                _ => "Internal Server Error",
            };

            let header = if status == 302 {
                let loc = if body.starts_with(b"Location: ") {
                    String::from_utf8_lossy(&body).trim().to_string()
                } else {
                    format!("Location: /tree/{}/\r\n", default_branch)
                };
                format!(
                    "HTTP/1.1 302 Found\r\n{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    loc
                )
            } else {
                format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    status_msg,
                    content_type,
                    body.len()
                )
            };

            let _ = socket.write_all(header.as_bytes()).await;
            if status != 302 {
                let _ = socket.write_all(&body).await;
            }
            let _ = socket.flush().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::blob_store::LocalBlobStore;
    use crate::cas::commit::{Commit, DirectoryManifest, ManifestEntry};
    use crate::metadata::LocalMetadataStore;
    use tempfile::tempdir;

    #[test]
    fn test_url_decode_and_query_parser() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("foo+bar"), "foo bar");
        assert_eq!(url_decode("%2Fsrc%2Flib.rs"), "/src/lib.rs");

        let params = parse_query_params("q=FastCDC&ref=feature%2F1&limit=25");
        assert_eq!(params.get("q").unwrap(), "FastCDC");
        assert_eq!(params.get("ref").unwrap(), "feature/1");
        assert_eq!(params.get("limit").unwrap(), "25");
    }

    #[test]
    fn test_web_routes_tree_blob_search_and_api() {
        let dir = tempdir().unwrap();
        let cas_dir = dir.path().join("cas");
        let meta_dir = dir.path().join("meta");
        let storage_dir = dir.path().join("storage");
        std::fs::create_dir_all(&cas_dir).unwrap();
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::create_dir_all(&storage_dir).unwrap();

        let blob_store = LocalBlobStore::new(&cas_dir).unwrap();
        let meta_store = LocalMetadataStore::new(&meta_dir).unwrap();

        // 1. Create file and tree
        let code_bytes = b"// Graft Source Code\npub fn hello() -> &'static str {\n    \"Graft\"\n}\n";
        let blob_digest = blob_store.put(code_bytes).unwrap();

        let readme_bytes = b"# Welcome to Graft\n\nHigh-performance virtualized source control.\n\n> [!NOTE]\n> Fast agent branching.\n";
        let readme_digest = blob_store.put(readme_bytes).unwrap();

        let sub_manifest = DirectoryManifest::default();
        let sub_digest = blob_store.put(&sub_manifest.serialize()).unwrap();

        let mut manifest = DirectoryManifest::default();
        manifest.entries.insert(
            "lib.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "lib.rs".to_string(),
                digest: blob_digest,
            },
        );
        manifest.entries.insert(
            "README.md".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "README.md".to_string(),
                digest: readme_digest,
            },
        );
        manifest.entries.insert(
            "src".to_string(),
            ManifestEntry::Directory {
                name: "src".to_string(),
                digest: sub_digest,
            },
        );
        let tree_bytes = manifest.serialize();
        let tree_digest = blob_store.put(&tree_bytes).unwrap();

        // 2. Create commit and register branch "refs/heads/main"
        let commit = Commit::new(
            Vec::new(),
            tree_digest,
            "Graft Developer <dev@graft.dev>".to_string(),
            "Initial commit for Graft Web".to_string(),
        );
        let commit_bytes = commit.serialize();
        let commit_digest = blob_store.put(&commit_bytes).unwrap();
        meta_store
            .compare_and_swap_ref("refs/heads/main", None, &commit_digest, "dev")
            .unwrap();

        // Test Route: Root redirect
        let (status, _, body) = route_request("/", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 302);
        assert!(String::from_utf8_lossy(&body).contains("Location: /tree/main/"));

        // Test Route: API Info
        let (status, ctype, body) = route_request("/api/info", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("json"));
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["service"], "Graft Web");
        assert_eq!(json["default_branch"], "main");

        // Test Route: API Tree (folders first)
        let (status, _, body) = route_request("/api/tree?ref=main", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["entries"][0]["name"], "src");
        assert_eq!(json["entries"][0]["type"], "directory");
        assert_eq!(json["entries"][1]["name"], "lib.rs");
        assert_eq!(json["entries"][2]["name"], "README.md");

        // Test Route: API Blob
        let (status, _, body) = route_request("/api/blob?ref=main&path=lib.rs", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["path"], "lib.rs");
        assert!(json["content"].as_str().unwrap().contains("pub fn hello()"));

        // Test Route: API Search (searches both code and markdown files)
        let (status, _, body) = route_request("/api/search?ref=main&q=Graft", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_matches"], 3); // 2 in lib.rs, 1 in README.md

        // Test Route: HTML Tree view (folders first & rich markdown README)
        let (status, ctype, body) = route_request("/tree/main/", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("lib.rs"));
        assert!(html.contains("src"));
        assert!(html.contains("README.md"));
        assert!(html.contains("Initial commit for Graft Web"));

        // Verify folder-first ordering: "src" appears in table before "lib.rs"
        let src_pos = html.find("href=\"/tree/main/src/\"").unwrap();
        let lib_pos = html.find("href=\"/blob/main/lib.rs\"").unwrap();
        assert!(src_pos < lib_pos, "Directories must appear before files");

        // Verify rich markdown rendering for README
        assert!(html.contains("class=\"markdown-body\""));
        assert!(html.contains("<h1 id=\"welcome-to-graft\">"));
        assert!(html.contains("markdown-alert markdown-alert-note"));

        // Test Route: HTML Blob view for .rs file
        let (status, ctype, body) = route_request("/blob/main/lib.rs", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("pub fn hello()"));
        assert!(html.contains("id=\"L1\""));

        // Test Route: HTML Blob view for .md file (preview/code tabs)
        let (status, ctype, body) = route_request("/blob/main/README.md", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("id=\"btn-preview\""));
        assert!(html.contains("id=\"btn-code\""));
        assert!(html.contains("id=\"blob-preview-container\""));
        assert!(html.contains("<h1 id=\"welcome-to-graft\">"));

        // Test Route: HTML Commits view
        let (status, _, body) = route_request("/commits?ref=main", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Initial commit for Graft Web"));
    }

    #[test]
    fn test_web_routes_blame_diff_and_pull_requests() {
        let dir = tempdir().unwrap();
        let cas_dir = dir.path().join("cas");
        let meta_dir = dir.path().join("meta");
        let storage_dir = dir.path().join("storage");
        std::fs::create_dir_all(&cas_dir).unwrap();
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::create_dir_all(&storage_dir).unwrap();

        let blob_store = LocalBlobStore::new(&cas_dir).unwrap();
        let meta_store = LocalMetadataStore::new(&meta_dir).unwrap();

        // 1. Initial Commit on "main"
        let file1_bytes = b"fn main() {\n    println!(\"Hello Graft\");\n}\n";
        let file1_digest = blob_store.put(file1_bytes).unwrap();

        let mut manifest1 = DirectoryManifest::default();
        manifest1.entries.insert(
            "main.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "main.rs".to_string(),
                digest: file1_digest,
            },
        );
        let tree1_digest = blob_store.put(&manifest1.serialize()).unwrap();

        let commit1 = Commit::new(
            Vec::new(),
            tree1_digest,
            "Alice <alice@graft.dev>".to_string(),
            "Initial main commit".to_string(),
        );
        let commit1_digest = blob_store.put(&commit1.serialize()).unwrap();
        meta_store
            .compare_and_swap_ref("refs/heads/main", None, &commit1_digest, "alice")
            .unwrap();

        // 2. Feature Commit on "feature-1"
        let file2_bytes = b"fn main() {\n    println!(\"Hello Graft\");\n    println!(\"Added Feature\");\n}\n";
        let file2_digest = blob_store.put(file2_bytes).unwrap();

        let mut manifest2 = DirectoryManifest::default();
        manifest2.entries.insert(
            "main.rs".to_string(),
            ManifestEntry::File {
                mode: "100644".to_string(),
                name: "main.rs".to_string(),
                digest: file2_digest,
            },
        );
        let tree2_digest = blob_store.put(&manifest2.serialize()).unwrap();

        let commit2 = Commit::new(
            vec![commit1_digest],
            tree2_digest,
            "Bob <bob@graft.dev>".to_string(),
            "Add feature printing".to_string(),
        );
        let commit2_digest = blob_store.put(&commit2.serialize()).unwrap();
        meta_store
            .compare_and_swap_ref("refs/heads/feature-1", None, &commit2_digest, "bob")
            .unwrap();

        // 3. Test Route: Git Blame HTML & API
        let (status, ctype, body) = route_request("/blame/main/main.rs", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("blame-table"));
        assert!(html.contains("Alice"));
        assert!(html.contains("Hello Graft"));

        let (status, ctype, body) = route_request("/api/blame?ref=main&path=main.rs", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("json"));
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_lines"], 3);
        assert_eq!(json["lines"][0]["author"], "Alice <alice@graft.dev>");

        // 4. Test Route: Split & Unified Diff HTML
        let (status, ctype, body) = route_request(&format!("/diff/{}", commit2_digest), &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(ctype.contains("html"));
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("diff-container"));
        assert!(html.contains("diff-split-table"));
        assert!(html.contains("diff-unified-table"));
        assert!(html.contains("setDiffMode"));

        // 5. Test Route: Pull Requests list & new page
        let (status, _, body) = route_request("/pulls", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("Pull Requests"));

        let (status, _, body) = route_request("/pulls/new", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("Compare &amp; Open Pull Request"));

        // 6. Test Route: Create PR via POST
        let pr_body = b"title=Add+feature+printing&description=Implements+cool+feature&author=Bob&source_branch=feature-1&target_branch=main";
        let (status, _, body) = route_request_full("POST", "/api/pulls/new", pr_body, &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 302);
        assert!(String::from_utf8_lossy(&body).contains("Location: /pull/1"));

        // 7. Test Route: View PR detail conversation & files tab
        let (status, _, body) = route_request("/pull/1", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Add feature printing"));
        assert!(html.contains("Implements cool feature"));
        assert!(html.contains("Merge Pull Request"));

        let (status, _, body) = route_request("/pull/1?tab=files", &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 200);
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("diff-container"));
        assert!(html.contains("Added Feature"));

        // 8. Test Route: Add Review Comment via POST
        let comment_body = b"pr_id=1&author=Reviewer&body=Looks+great+LGTM!";
        let (status, _, body) = route_request_full("POST", "/api/pulls/comment", comment_body, &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 302);
        assert!(String::from_utf8_lossy(&body).contains("Location: /pull/1"));

        // Verify comment rendered in PR conversation
        let (_, _, body) = route_request("/pull/1", &blob_store, &meta_store, &storage_dir, "main");
        assert!(String::from_utf8_lossy(&body).contains("Looks great LGTM!"));

        // 9. Test Route: Merge PR via POST
        let merge_body = b"pr_id=1";
        let (status, _, body) = route_request_full("POST", "/api/pulls/merge", merge_body, &blob_store, &meta_store, &storage_dir, "main");
        assert_eq!(status, 302);
        assert!(String::from_utf8_lossy(&body).contains("Location: /pull/1"));

        // Verify PR status updated to Merged
        let (_, _, body) = route_request("/pull/1", &blob_store, &meta_store, &storage_dir, "main");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Merged"));
        assert!(html.contains("Pull Request successfully merged into"));
    }
}
