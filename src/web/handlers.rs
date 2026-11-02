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

//! HTTP Route Handlers for Repository Browsing, Code Search, Diffs, and Workspaces.

use crate::cas::blob_store::BlobStore;
use crate::cas::commit::{get_commit_history, Commit, DirectoryManifest, ManifestEntry};
use crate::cas::digest::Digest;
use crate::git::{diff_trees, diff_workspace};
use crate::git::diff::{diff_trees_report, diff_workspace_report};
use crate::metadata::{resolve_ref, MetadataStore};
use crate::review::ReviewManager;
use crate::web::search::{get_index_stats, get_trigram_index, read_entry_bytes, search_level1, search_level2};
use crate::web::templates::*;
use crate::workspace::WorkspaceManager;
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// HTTP Response tuple: (status_code, content_type, body_bytes)
pub type HttpResponse = (u16, &'static str, Vec<u8>);

/// Helper to construct JSON response.
pub fn json_response(val: serde_json::Value) -> HttpResponse {
    let body = serde_json::to_vec_pretty(&val).unwrap_or_default();
    (200, "application/json; charset=utf-8", body)
}

/// Helper to construct HTML response.
pub fn html_response(html: String) -> HttpResponse {
    (200, "text/html; charset=utf-8", html.into_bytes())
}

/// Helper to construct 302 Redirect response.
pub fn redirect_response(location: &str) -> HttpResponse {
    (
        302,
        "text/html; charset=utf-8",
        format!("Location: {}\r\n", location).into_bytes(),
    )
}

/// Helper to construct plain text error response.
pub fn not_found_response(msg: &str) -> HttpResponse {
    (404, "text/plain; charset=utf-8", format!("404 Not Found: {}\n", msg).into_bytes())
}

/// Helper to construct internal server error response.
pub fn error_response(msg: &str) -> HttpResponse {
    (500, "text/plain; charset=utf-8", format!("500 Internal Server Error: {}\n", msg).into_bytes())
}

/// Resolves a revision string (branch name, commit hex, or digest) to a root tree digest and commit object.
pub fn resolve_root_tree(
    spec: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
) -> io::Result<(Option<Digest>, Option<Commit>, Digest)> {
    // 1. Try parsing as full digest (<hash>:<size>)
    if let Ok(d) = Digest::parse(spec) {
        if let Some(bytes) = blob_store.get(&d)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((Some(d), Some(commit.clone()), commit.root_tree_digest));
            }
            // Or it could be a direct tree digest
            return Ok((None, None, d));
        }
    }

    // 2. Try parsing as 64-char hex
    if spec.len() == 64 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        let d = Digest::from_hex(spec, 0)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if let Some(bytes) = blob_store.get(&d)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((Some(d), Some(commit.clone()), commit.root_tree_digest));
            }
            return Ok((None, None, d));
        }
    }

    // 2b. Try resolving by short hex prefix (7+ chars) across known branches & commit history
    if spec.len() >= 7 && spec.len() <= 64 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(branches) = meta_store.list_refs("refs/heads/") {
            for (_ref_name, commit_digest) in branches {
                if commit_digest.to_hex().starts_with(spec) {
                    if let Ok(Some(bytes)) = blob_store.get(&commit_digest) {
                        if let Ok(commit) = Commit::deserialize(&bytes) {
                            return Ok((Some(commit_digest), Some(commit.clone()), commit.root_tree_digest));
                        }
                    }
                }
                if let Ok(history) = get_commit_history(&commit_digest, Some(100), blob_store) {
                    for (digest, commit) in history {
                        if digest.to_hex().starts_with(spec) {
                            return Ok((Some(digest), Some(commit.clone()), commit.root_tree_digest));
                        }
                    }
                }
            }
        }
    }

    // 3. Try resolving as a branch in metadata store
    if let Some((_canonical_ref, commit_digest)) = resolve_ref(meta_store, spec)? {
        if let Some(bytes) = blob_store.get(&commit_digest)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((Some(commit_digest), Some(commit.clone()), commit.root_tree_digest));
            } else {
                return Ok((None, None, commit_digest));
            }
        }
    }

    // 4. Fallback: if "main" or default was requested, check any available branch in metadata store
    let branches = meta_store.list_refs("refs/heads/")?;
    if let Some((_first_ref, commit_digest)) = branches.first() {
        if let Some(bytes) = blob_store.get(commit_digest)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((Some(*commit_digest), Some(commit.clone()), commit.root_tree_digest));
            } else {
                return Ok((None, None, *commit_digest));
            }
        }
    }

    // 5. Ultimate Fallback: scan CAS for latest commit or directory manifest
    let blobs = blob_store.list_blobs()?;
    for info in blobs.iter().rev() {
        if let Some(bytes) = blob_store.get(&info.digest)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((Some(info.digest), Some(commit.clone()), commit.root_tree_digest));
            }
        }
    }
    let mut manifests = Vec::new();
    let mut child_dirs = std::collections::HashSet::new();

    for info in &blobs {
        if let Some(bytes) = blob_store.get(&info.digest)? {
            let str_repr = String::from_utf8_lossy(&bytes);
            if let Some(first_line) = str_repr.lines().next() {
                let trimmed_line = first_line.trim();
                if trimmed_line.starts_with("FILE ") || trimmed_line.starts_with("DIR ") || trimmed_line.starts_with("CHUNKED ") {
                    if let Ok(manifest) = DirectoryManifest::from_bytes(&bytes) {
                        for entry in manifest.entries.values() {
                            if let ManifestEntry::Directory { digest, .. } = entry {
                                child_dirs.insert(*digest);
                            }
                        }
                        manifests.push(info.digest);
                    }
                }
            }
        }
    }

    for digest in manifests.iter().rev() {
        if !child_dirs.contains(digest) {
            return Ok((None, None, *digest));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Unable to resolve revision, branch, or tree '{}'", spec),
    ))
}

/// Traverses down a Merkle tree along a relative path, returning the final DirectoryManifest or ManifestEntry.
pub fn traverse_tree_path(
    root_digest: &Digest,
    path_str: &str,
    blob_store: &dyn BlobStore,
) -> io::Result<Result<DirectoryManifest, ManifestEntry>> {
    let trimmed = path_str.trim_matches('/');
    if trimmed.is_empty() {
        let bytes = blob_store.get(root_digest)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Root tree {} not found in CAS", root_digest))
        })?;
        let manifest = DirectoryManifest::from_bytes(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        return Ok(Ok(manifest));
    }

    let parts: Vec<&str> = trimmed.split('/').collect();
    let mut current_dir_digest = *root_digest;

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let bytes = blob_store.get(&current_dir_digest)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Directory tree {} not found", current_dir_digest))
        })?;
        let manifest = DirectoryManifest::from_bytes(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let entry = manifest.entries.get(*part).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("Path component '{}' not found", part))
        })?;

        match entry {
            ManifestEntry::Directory { digest, .. } => {
                if is_last {
                    let sub_bytes = blob_store.get(digest)?.ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("Subdirectory {} not found", digest))
                    })?;
                    let sub_manifest = DirectoryManifest::from_bytes(&sub_bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    return Ok(Ok(sub_manifest));
                } else {
                    current_dir_digest = *digest;
                }
            }
            ManifestEntry::File { .. } | ManifestEntry::ChunkedFile { .. } => {
                if is_last {
                    return Ok(Err(entry.clone()));
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Component '{}' is a file, cannot traverse further", part),
                    ));
                }
            }
        }
    }

    let bytes = blob_store.get(&current_dir_digest)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("Tree {} not found", current_dir_digest))
    })?;
    let manifest = DirectoryManifest::from_bytes(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Ok(manifest))
}

/// Helper to extract entry digest from a DirectoryManifest.
fn get_manifest_entry_digest(manifest: &DirectoryManifest, name: &str) -> Option<Digest> {
    manifest.entries.get(name).map(|e| match e {
        ManifestEntry::Directory { digest, .. } => *digest,
        ManifestEntry::File { digest, .. } => *digest,
        ManifestEntry::ChunkedFile { manifest_digest, .. } => *manifest_digest,
    })
}

/// Helper to load a DirectoryManifest at `subpath` starting from `root_tree`.
fn load_dir_manifest(
    root_tree: &Digest,
    subpath: &str,
    blob_store: &dyn BlobStore,
) -> Option<DirectoryManifest> {
    let clean = subpath.trim_matches('/');
    if clean.is_empty() {
        let bytes = blob_store.get(root_tree).ok()??;
        DirectoryManifest::from_bytes(&bytes).ok()
    } else {
        match traverse_tree_path(root_tree, clean, blob_store) {
            Ok(Ok(m)) => Some(m),
            _ => None,
        }
    }
}

/// Finds the most recent commit summary, commit short hash, and relative age for each entry in a directory.
fn resolve_entries_commit_info(
    head_commit_digest: Option<&Digest>,
    head_commit: Option<&Commit>,
    current_path: &str,
    entry_names: &[String],
    blob_store: &dyn BlobStore,
) -> HashMap<String, (String, String, String)> {
    let mut results = HashMap::new();
    let head_digest = match head_commit_digest {
        Some(d) => d,
        None => return results,
    };
    let head = match head_commit {
        Some(c) => c,
        None => return results,
    };

    let default_msg = head.message.lines().next().unwrap_or("Commit").to_string();
    let default_hash = head_digest.short_hex();
    let default_age = format_time_ago(head.timestamp_seconds);

    // Load recent history (up to 40 commits)
    let history = get_commit_history(head_digest, Some(40), blob_store).unwrap_or_default();
    if history.is_empty() {
        for name in entry_names {
            results.insert(
                name.clone(),
                (default_msg.clone(), default_hash.clone(), default_age.clone()),
            );
        }
        return results;
    }

    let mut unassigned: std::collections::HashSet<String> =
        entry_names.iter().cloned().collect();

    for (commit_digest, commit) in &history {
        if unassigned.is_empty() {
            break;
        }

        let cur_dir = match load_dir_manifest(&commit.root_tree_digest, current_path, blob_store) {
            Some(m) => m,
            None => continue,
        };

        let parent_dir = if let Some(parent_digest) = commit.parent_commits.first() {
            if let Ok(Some(p_bytes)) = blob_store.get(parent_digest) {
                if let Ok(p_commit) = Commit::deserialize(&p_bytes) {
                    load_dir_manifest(&p_commit.root_tree_digest, current_path, blob_store)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let summary = commit.message.lines().next().unwrap_or("Commit").to_string();
        let short_hash = commit_digest.short_hex();
        let age = format_time_ago(commit.timestamp_seconds);

        let mut matched = Vec::new();
        for name in &unassigned {
            let cur_d = get_manifest_entry_digest(&cur_dir, name);
            let parent_d = parent_dir
                .as_ref()
                .and_then(|p| get_manifest_entry_digest(p, name));

            // If present in current and differed from parent, this commit introduced or modified it
            if cur_d.is_some() && cur_d != parent_d {
                results.insert(
                    name.clone(),
                    (summary.clone(), short_hash.clone(), age.clone()),
                );
                matched.push(name.clone());
            }
        }

        for m in matched {
            unassigned.remove(&m);
        }
    }

    // Any remaining entries default to head commit info
    for name in unassigned {
        results.insert(
            name,
            (default_msg.clone(), default_hash.clone(), default_age.clone()),
        );
    }

    results
}

/// Handler for `/tree/{ref}/{path...}` and `/api/tree`.
pub fn handle_tree(
    ref_name: &str,
    path_str: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let (commit_digest, commit_opt, root_tree) =
        match resolve_root_tree(ref_name, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };

    let traversal = match traverse_tree_path(&root_tree, path_str, blob_store) {
        Ok(t) => t,
        Err(e) => return not_found_response(&e.to_string()),
    };

    let manifest = match traversal {
        Ok(m) => m,
        Err(entry) => {
            // Path was actually a file; redirect or serve blob
            return handle_blob_entry(
                ref_name, path_str, &entry, blob_store, meta_store, false, is_api,
            );
        }
    };

    let entry_names: Vec<String> = manifest.entries.keys().cloned().collect();
    let commit_info = resolve_entries_commit_info(
        commit_digest.as_ref(),
        commit_opt.as_ref(),
        path_str,
        &entry_names,
        blob_store,
    );

    if is_api {
        let mut entries_json: Vec<_> = manifest
            .entries
            .iter()
            .map(|(name, entry)| {
                let (commit_msg, commit_hash, commit_age) =
                    commit_info.get(name).cloned().unwrap_or_default();
                match entry {
                    ManifestEntry::Directory { digest, .. } => json!({
                        "name": name,
                        "type": "directory",
                        "digest": digest.to_string(),
                        "commit_msg": commit_msg,
                        "commit_hash": commit_hash,
                        "commit_age": commit_age,
                    }),
                    ManifestEntry::File { mode, digest, .. } => json!({
                        "name": name,
                        "type": "file",
                        "mode": mode,
                        "digest": digest.to_string(),
                        "commit_msg": commit_msg,
                        "commit_hash": commit_hash,
                        "commit_age": commit_age,
                    }),
                    ManifestEntry::ChunkedFile { mode, manifest_digest, total_size, .. } => json!({
                        "name": name,
                        "type": "chunked_file",
                        "mode": mode,
                        "manifest_digest": manifest_digest.to_string(),
                        "size": total_size,
                        "commit_msg": commit_msg,
                        "commit_hash": commit_hash,
                        "commit_age": commit_age,
                    }),
                }
            })
            .collect();

        // Sort folder first, then alphabetical (case-insensitive)
        entries_json.sort_by(|a, b| {
            let a_is_dir = a["type"] == "directory";
            let b_is_dir = b["type"] == "directory";
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()),
            }
        });

        return json_response(json!({
            "ref": ref_name,
            "path": path_str,
            "root_tree": root_tree.to_string(),
            "commit": commit_digest.map(|d| d.to_string()),
            "entries": entries_json,
        }));
    }

    let mut table_entries = Vec::new();
    let trimmed = path_str.trim_matches('/');

    // Collect README content if present
    let mut readme_content = None;

    for (name, entry) in &manifest.entries {
        let full_path = if trimmed.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", trimmed, name)
        };

        let (commit_msg, commit_hash, commit_age) =
            commit_info.get(name).cloned().unwrap_or_default();

        match entry {
            ManifestEntry::Directory { digest, .. } => {
                table_entries.push(TreeTableEntry {
                    name: name.clone(),
                    full_path,
                    is_dir: true,
                    is_chunked: false,
                    size: 0,
                    digest: digest.to_string(),
                    commit_msg,
                    commit_hash,
                    commit_age,
                });
            }
            ManifestEntry::File { digest, .. } => {
                let size = blob_store.get(digest).ok().flatten().map(|b| b.len() as u64).unwrap_or(0);
                if name.to_lowercase() == "readme.md" && readme_content.is_none() {
                    if let Ok(Some(bytes)) = blob_store.get(digest) {
                        readme_content = Some(String::from_utf8_lossy(&bytes).to_string());
                    }
                }
                table_entries.push(TreeTableEntry {
                    name: name.clone(),
                    full_path,
                    is_dir: false,
                    is_chunked: false,
                    size,
                    digest: digest.to_string(),
                    commit_msg,
                    commit_hash,
                    commit_age,
                });
            }
            ManifestEntry::ChunkedFile { manifest_digest, total_size, .. } => {
                table_entries.push(TreeTableEntry {
                    name: name.clone(),
                    full_path,
                    is_dir: false,
                    is_chunked: true,
                    size: *total_size,
                    digest: manifest_digest.to_string(),
                    commit_msg,
                    commit_hash,
                    commit_age,
                });
            }
        }
    }

    // GitHub folder-first ordering:
    // Directories first (alphabetical A-Z), then files (alphabetical A-Z)
    table_entries.sort_by(|a, b| {
        match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });

    let commit_ref = match (commit_digest.as_ref(), commit_opt.as_ref()) {
        (Some(d), Some(c)) => {
            let stats_opt = if let Some(parent_commit_digest) = c.parent_commits.first() {
                let parent_tree = crate::cas::commit::get_commit_tree_digest(parent_commit_digest, blob_store);
                crate::git::diff::diff_trees(&parent_tree, &c.root_tree_digest, blob_store)
                    .ok()
                    .map(|diff| crate::git::diff::compute_diff_stats(&diff))
            } else {
                None
            };
            Some((d, c, stats_opt))
        }
        _ => None,
    };

    let branches = get_branch_names(meta_store);
    let body = render_tree_view(
        ref_name,
        &branches,
        path_str,
        commit_ref,
        &table_entries,
        readme_content.as_deref(),
    );
    html_response(render_html_page(
        &format!("Tree: {}", path_str),
        "files",
        ref_name,
        &body,
    ))
}

/// Handler for `/blob/{ref}/{path...}` and `/raw/{ref}/{path...}`.
pub fn handle_blob(
    ref_name: &str,
    path_str: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    raw: bool,
    is_api: bool,
) -> HttpResponse {
    let (_commit_digest, _commit_opt, root_tree) =
        match resolve_root_tree(ref_name, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };

    let traversal = match traverse_tree_path(&root_tree, path_str, blob_store) {
        Ok(t) => t,
        Err(e) => return not_found_response(&e.to_string()),
    };

    match traversal {
        Ok(_) => not_found_response("Path is a directory, not a file. Use /tree/ instead."),
        Err(entry) => handle_blob_entry(
            ref_name, path_str, &entry, blob_store, meta_store, raw, is_api,
        ),
    }
}

fn get_branch_names(meta_store: &dyn MetadataStore) -> Vec<String> {
    meta_store
        .list_refs("refs/heads/")
        .unwrap_or_default()
        .into_iter()
        .map(|(r, _)| r.strip_prefix("refs/heads/").unwrap_or(&r).to_string())
        .collect()
}

fn handle_blob_entry(
    ref_name: &str,
    path_str: &str,
    entry: &ManifestEntry,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    raw: bool,
    is_api: bool,
) -> HttpResponse {
    let (digest_str, is_chunked) = match entry {
        ManifestEntry::File { digest, .. } => (digest.to_string(), false),
        ManifestEntry::ChunkedFile {
            manifest_digest, ..
        } => (manifest_digest.to_string(), true),
        ManifestEntry::Directory { digest, .. } => (digest.to_string(), false),
    };

    let bytes = match read_entry_bytes(entry, blob_store, 20 * 1024 * 1024) {
        Ok(b) => b,
        Err(e) => return error_response(&format!("Failed to read file from CAS: {}", e)),
    };

    if raw {
        return (200, "text/plain; charset=utf-8", bytes);
    }

    if is_api {
        let is_binary = crate::web::search::is_binary_content(&bytes);
        return json_response(json!({
            "ref": ref_name,
            "path": path_str,
            "digest": digest_str,
            "is_chunked": is_chunked,
            "size": bytes.len(),
            "is_binary": is_binary,
            "content": if is_binary { serde_json::Value::Null } else { serde_json::Value::String(String::from_utf8_lossy(&bytes).to_string()) },
        }));
    }

    let branches = get_branch_names(meta_store);
    let body = render_blob_view(
        ref_name,
        &branches,
        path_str,
        &bytes,
        &digest_str,
        is_chunked,
    );
    html_response(render_html_page(
        &format!("Blob: {}", path_str),
        "files",
        ref_name,
        &body,
    ))
}

/// Handler for `/search?q=...&ref=...` and `/api/search`.
/// Handler for `/search` and `/api/search` with Level-1 & Level-2 support.
pub fn handle_search(
    query: &str,
    ref_name: &str,
    path_filter: Option<&str>,
    engine: &str,
    is_regex: bool,
    is_case_sensitive: bool,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let (_commit_digest, _commit_opt, root_tree) =
        match resolve_root_tree(ref_name, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };

    let search_res = if engine == "level1" {
        match search_level1(
            &root_tree,
            ref_name,
            query,
            path_filter,
            is_regex,
            is_case_sensitive,
            blob_store,
            100,
        ) {
            Ok(r) => r,
            Err(e) => return error_response(&format!("Search failed: {}", e)),
        }
    } else {
        match search_level2(
            &root_tree,
            ref_name,
            query,
            path_filter,
            is_regex,
            is_case_sensitive,
            blob_store,
            100,
        ) {
            Ok(r) => r,
            Err(e) => return error_response(&format!("Search failed: {}", e)),
        }
    };

    if is_api {
        return json_response(serde_json::to_value(&search_res).unwrap_or_default());
    }

    if engine != "level1" && query.trim().is_empty() {
        let needs_indexing = {
            let idx = get_trigram_index().read().unwrap();
            !idx.is_tree_indexed(&root_tree)
        };
        if needs_indexing {
            if let Ok(mut idx) = get_trigram_index().write() {
                let _ = idx.index_tree(&root_tree, blob_store);
            }
        }
    }

    let branches = get_branch_names(meta_store);
    let stats = get_index_stats();
    let body = render_search_view(ref_name, &branches, &search_res, &stats);
    let title = if query.trim().is_empty() {
        "Code Search".to_string()
    } else {
        format!("Search: {}", query)
    };
    html_response(render_html_page(
        &title,
        "search",
        ref_name,
        &body,
    ))
}

/// Handler for `/commits?ref=...` and `/api/commits`.
pub fn handle_commits(
    ref_name: &str,
    limit: usize,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let (commit_digest_opt, _commit_opt, _root_tree) =
        match resolve_root_tree(ref_name, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };

    let head_commit = match commit_digest_opt {
        Some(d) => d,
        None => return not_found_response("Specified ref does not resolve to a commit"),
    };

    let history = match get_commit_history(&head_commit, Some(limit), blob_store) {
        Ok(h) => h,
        Err(e) => return error_response(&format!("Failed to retrieve commit history: {}", e)),
    };

    let mut commit_items = Vec::new();
    for (digest, commit) in &history {
        let stats = if let Some(parent_commit_digest) = commit.parent_commits.first() {
            let parent_tree = crate::cas::commit::get_commit_tree_digest(parent_commit_digest, blob_store);
            if let Ok(diff) = crate::git::diff::diff_trees(&parent_tree, &commit.root_tree_digest, blob_store) {
                crate::git::diff::compute_diff_stats(&diff)
            } else {
                crate::git::diff::DiffStats::default()
            }
        } else {
            crate::git::diff::DiffStats::default()
        };
        commit_items.push((digest.clone(), commit.clone(), stats));
    }

    if is_api {
        let history_json: Vec<_> = commit_items
            .iter()
            .map(|(d, c, s)| json!({
                "commit": d.to_string(),
                "root_tree": c.root_tree_digest.to_string(),
                "author": c.author,
                "timestamp_seconds": c.timestamp_seconds,
                "parents": c.parent_commits.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                "message": c.message,
                "additions": s.additions,
                "deletions": s.deletions,
                "files_changed": s.files_changed,
            }))
            .collect();
        return json_response(json!({
            "ref": ref_name,
            "commits": history_json,
        }));
    }

    let body = render_commits_view(ref_name, &commit_items);
    html_response(render_html_page(
        &format!("Commits: {}", ref_name),
        "commits",
        ref_name,
        &body,
    ))
}

/// Handler for `/diff/{commit}` and `/diff?base=...&target=...`.
pub fn handle_diff(
    spec_a: &str,
    spec_b_opt: Option<&str>,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let (tree_a, tree_b, title) = if let Some(spec_b) = spec_b_opt {
        let (_, _, t_a) = match resolve_root_tree(spec_a, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };
        let (_, _, t_b) = match resolve_root_tree(spec_b, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };
        (t_a, t_b, format!("{}..{}", spec_a, spec_b))
    } else {
        // Single commit: diff against its parent
        let (_, commit_opt, commit_tree) = match resolve_root_tree(spec_a, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };
        let commit = match commit_opt {
            Some(c) => c,
            None => return not_found_response("Commit object required for single-argument diff"),
        };
        let parent_tree = if let Some(parent_digest) = commit.parent_commits.first() {
            if let Ok(Some(parent_bytes)) = blob_store.get(parent_digest) {
                if let Ok(parent_commit) = Commit::deserialize(&parent_bytes) {
                    parent_commit.root_tree_digest
                } else {
                    Digest::from_bytes(b"")
                }
            } else {
                Digest::from_bytes(b"")
            }
        } else {
            Digest::from_bytes(b"")
        };
        (parent_tree, commit_tree, format!("Commit {}", spec_a))
    };

    let diff_report = match diff_trees_report(&tree_a, &tree_b, blob_store) {
        Ok(r) => r,
        Err(e) => return error_response(&format!("Diff computation failed: {}", e)),
    };

    if is_api {
        let diff_text = match diff_trees(&tree_a, &tree_b, blob_store) {
            Ok(d) => d,
            Err(e) => return error_response(&format!("Diff computation failed: {}", e)),
        };
        return json_response(json!({
            "title": title,
            "base_tree": tree_a.to_string(),
            "target_tree": tree_b.to_string(),
            "diff": diff_text,
            "total_files_changed": diff_report.total_stats.files_changed,
            "total_additions": diff_report.total_stats.additions,
            "total_deletions": diff_report.total_stats.deletions,
        }));
    }

    let body = render_diff_view(&title, &diff_report);
    html_response(render_html_page(&title, "diff", spec_a, &body))
}

/// Handler for `/workspaces` and `/api/workspaces`.
pub fn handle_workspaces(
    storage_dir: &Path,
    _blob_store: &dyn BlobStore,
    is_api: bool,
) -> HttpResponse {
    let manager = WorkspaceManager::new(storage_dir);
    let workspaces = manager.list_workspaces().unwrap_or_default();

    if is_api {
        let ws_json: Vec<_> = workspaces
            .iter()
            .map(|ws| json!({
                "id": ws.id,
                "commit": ws.commit,
                "branch": ws.branch,
                "base_commit": ws.base_commit,
                "agent_id": ws.agent_id,
                "mount_backend": ws.mount_backend,
                "mount_pid": ws.mount_pid,
                "mount_target": ws.mount_target.as_ref().map(|p| p.display().to_string()),
                "lower_dir": ws.lower_dir.display().to_string(),
                "upper_dir": ws.upper_dir.display().to_string(),
            }))
            .collect();
        return json_response(json!({
            "storage_dir": storage_dir.display().to_string(),
            "workspaces": ws_json,
        }));
    }

    let body = render_workspaces_view(&workspaces, &storage_dir.display().to_string());
    html_response(render_html_page("Workspaces", "workspaces", "main", &body))
}

/// Handler for `/workspaces/{id}/diff` and `/api/workspaces/{id}/diff`.
pub fn handle_workspace_diff(
    workspace_id: &str,
    storage_dir: &Path,
    blob_store: &dyn BlobStore,
    is_api: bool,
) -> HttpResponse {
    let manager = WorkspaceManager::new(storage_dir);
    let config = match manager.load_config(workspace_id) {
        Ok(c) => c,
        Err(e) => return not_found_response(&format!("Workspace '{}' not found: {}", workspace_id, e)),
    };

    let diff_report = match diff_workspace_report(&config, blob_store) {
        Ok(r) => r,
        Err(e) => return error_response(&format!("Failed to diff workspace '{}': {}", workspace_id, e)),
    };

    let title = format!("Workspace '{}' Live Changes", workspace_id);
    if is_api {
        let diff_text = match diff_workspace(&config, blob_store) {
            Ok(d) => d,
            Err(_) => String::new(),
        };
        return json_response(json!({
            "workspace_id": workspace_id,
            "tracked_branch": config.branch,
            "base_commit": config.base_commit.as_ref().unwrap_or(&config.commit),
            "diff": diff_text,
            "total_files_changed": diff_report.total_stats.files_changed,
            "total_additions": diff_report.total_stats.additions,
            "total_deletions": diff_report.total_stats.deletions,
        }));
    }

    let body = render_diff_view(&title, &diff_report);
    html_response(render_html_page(&title, "workspaces", "main", &body))
}

/// Handler for `/blame/{ref}/{path...}` and `/api/blame`.
pub fn handle_blame(
    ref_name: &str,
    path_str: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let (commit_digest_opt, _commit_opt, _root_tree) =
        match resolve_root_tree(ref_name, blob_store, meta_store) {
            Ok(res) => res,
            Err(e) => return not_found_response(&e.to_string()),
        };

    let commit_digest = match commit_digest_opt {
        Some(d) => d,
        None => return not_found_response(&format!("Cannot compute blame: '{}' does not resolve to a commit", ref_name)),
    };

    let blame = match crate::git::blame::compute_blame(path_str, &commit_digest, blob_store) {
        Ok(b) => b,
        Err(e) => return error_response(&format!("Blame computation failed: {}", e)),
    };

    if is_api {
        let lines_json: Vec<_> = blame
            .lines
            .iter()
            .map(|l| {
                json!({
                    "line_no": l.line_no,
                    "commit_digest": l.commit_digest,
                    "short_hash": l.short_hash,
                    "author": l.author,
                    "summary": l.summary,
                    "timestamp": l.timestamp,
                    "content": l.content,
                })
            })
            .collect();
        return json_response(json!({
            "ref": ref_name,
            "path": path_str,
            "total_lines": blame.lines.len(),
            "lines": lines_json,
        }));
    }

    let branches = get_branch_names(meta_store);
    let body = render_blame_view(ref_name, &branches, path_str, &blame);
    let title = format!("Blame: {}", path_str);
    html_response(render_html_page(&title, "blame", ref_name, &body))
}

/// Handler for `/pulls` and `/api/pulls`.
pub fn handle_pulls(
    storage_dir: &Path,
    _blob_store: &dyn BlobStore,
    _meta_store: &dyn MetadataStore,
    default_branch: &str,
    is_api: bool,
) -> HttpResponse {
    let review_mgr = ReviewManager::new(storage_dir);
    let prs = review_mgr.list_prs().unwrap_or_default();

    if is_api {
        let pr_json: Vec<_> = prs
            .iter()
            .map(|pr| {
                json!({
                    "id": pr.id,
                    "title": pr.title,
                    "description": pr.description,
                    "author": pr.author,
                    "source_branch": pr.source_branch,
                    "target_branch": pr.target_branch,
                    "status": format!("{:?}", pr.status),
                    "head_commit": pr.head_commit.to_string(),
                    "base_commit": pr.base_commit.to_string(),
                    "merge_commit": pr.merge_commit.as_ref().map(|d| d.to_string()),
                    "created_at": pr.created_at,
                    "updated_at": pr.updated_at,
                    "comments_count": pr.comments.len(),
                })
            })
            .collect();
        return json_response(json!({
            "pull_requests": pr_json,
        }));
    }

    let body = render_pull_requests_view(&prs, default_branch);
    html_response(render_html_page("Pull Requests", "pulls", default_branch, &body))
}

/// Handler for `GET /pulls/new`.
pub fn handle_new_pr_page(
    meta_store: &dyn MetadataStore,
    default_branch: &str,
) -> HttpResponse {
    let branches = get_branch_names(meta_store);
    let body = render_new_pr_view(&branches, default_branch);
    html_response(render_html_page("New Pull Request", "pulls", default_branch, &body))
}

/// Handler for `/pull/{id}` and `/api/pulls/{id}`.
pub fn handle_pull_detail(
    pr_id: usize,
    active_subtab: &str,
    storage_dir: &Path,
    blob_store: &dyn BlobStore,
    _meta_store: &dyn MetadataStore,
    default_branch: &str,
    is_api: bool,
) -> HttpResponse {
    let review_mgr = ReviewManager::new(storage_dir);
    let pr = match review_mgr.get_pr(pr_id) {
        Ok(Some(p)) => p,
        Ok(None) => return not_found_response(&format!("Pull Request #{} not found", pr_id)),
        Err(e) => return error_response(&format!("Failed to load PR #{}: {}", pr_id, e)),
    };

    // Compute diff report between base_commit and head_commit
    let diff_report = (|| -> Option<crate::git::diff::DiffReport> {
        let base_commit_bytes = blob_store.get(&pr.base_commit).ok()??;
        let base_commit = Commit::deserialize(&base_commit_bytes).ok()?;
        let head_commit_bytes = blob_store.get(&pr.head_commit).ok()??;
        let head_commit = Commit::deserialize(&head_commit_bytes).ok()?;
        diff_trees_report(
            &base_commit.root_tree_digest,
            &head_commit.root_tree_digest,
            blob_store,
        )
        .ok()
    })();

    if is_api {
        let comments_json: Vec<_> = pr
            .comments
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "author": c.author,
                    "body": c.body,
                    "created_at": c.created_at,
                    "resolved": c.resolved,
                    "file_path": c.file_path,
                    "line_no": c.line_no,
                })
            })
            .collect();

        return json_response(json!({
            "id": pr.id,
            "title": pr.title,
            "description": pr.description,
            "author": pr.author,
            "source_branch": pr.source_branch,
            "target_branch": pr.target_branch,
            "status": format!("{:?}", pr.status),
            "head_commit": pr.head_commit.to_string(),
            "base_commit": pr.base_commit.to_string(),
            "merge_commit": pr.merge_commit.as_ref().map(|d| d.to_string()),
            "created_at": pr.created_at,
            "updated_at": pr.updated_at,
            "comments": comments_json,
            "diff_summary": diff_report.as_ref().map(|r| json!({
                "files_changed": r.total_stats.files_changed,
                "additions": r.total_stats.additions,
                "deletions": r.total_stats.deletions,
            })),
        }));
    }

    let title = format!("PR #{}: {}", pr.id, pr.title);
    let body = render_pull_request_detail_view(&pr, diff_report.as_ref(), active_subtab, default_branch);
    html_response(render_html_page(&title, "pulls", default_branch, &body))
}

/// Handler for `POST /api/pulls/new`.
pub fn handle_create_pr_post(
    body_params: &HashMap<String, String>,
    storage_dir: &Path,
    _blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let title = body_params.get("title").map(|s| s.as_str()).unwrap_or("").trim();
    let description = body_params.get("description").map(|s| s.as_str()).unwrap_or("").trim();
    let author = body_params.get("author").map(|s| s.as_str()).unwrap_or("AI Agent <agent@graft.dev>").trim();
    let source_branch = body_params.get("source_branch").map(|s| s.as_str()).unwrap_or("").trim();
    let target_branch = body_params.get("target_branch").map(|s| s.as_str()).unwrap_or("").trim();

    if title.is_empty() || source_branch.is_empty() || target_branch.is_empty() {
        return error_response("Missing required fields: title, source_branch, or target_branch");
    }

    // Resolve source branch commit
    let source_ref = crate::metadata::normalize_ref_name(source_branch);
    let head_commit = match meta_store.get_ref(&source_ref) {
        Ok(Some(d)) => d,
        _ => return not_found_response(&format!("Source branch '{}' not found", source_branch)),
    };

    // Resolve target branch commit
    let target_ref = crate::metadata::normalize_ref_name(target_branch);
    let base_commit = match meta_store.get_ref(&target_ref) {
        Ok(Some(d)) => d,
        _ => return not_found_response(&format!("Target branch '{}' not found", target_branch)),
    };

    let review_mgr = ReviewManager::new(storage_dir);
    match review_mgr.create_pr(
        title,
        description,
        author,
        source_branch,
        target_branch,
        head_commit,
        base_commit,
    ) {
        Ok(pr) => {
            if is_api {
                json_response(json!({
                    "status": "created",
                    "pr_id": pr.id,
                    "title": pr.title,
                }))
            } else {
                redirect_response(&format!("/pull/{}", pr.id))
            }
        }
        Err(e) => error_response(&format!("Failed to create PR: {}", e)),
    }
}

/// Handler for `POST /api/pulls/comment`.
pub fn handle_pr_comment_post(
    body_params: &HashMap<String, String>,
    storage_dir: &Path,
    is_api: bool,
) -> HttpResponse {
    let pr_id: usize = match body_params.get("pr_id").and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return error_response("Missing or invalid pr_id"),
    };
    let author = body_params.get("author").map(|s| s.as_str()).unwrap_or("Reviewer <reviewer@graft.dev>").trim();
    let body = body_params.get("body").map(|s| s.as_str()).unwrap_or("").trim();
    let file_path = body_params.get("file_path").map(|s| s.as_str()).filter(|s| !s.is_empty());
    let line_no = body_params.get("line_no").and_then(|s| s.parse().ok());

    if body.is_empty() {
        return error_response("Comment body cannot be empty");
    }

    let review_mgr = ReviewManager::new(storage_dir);
    match review_mgr.add_comment(pr_id, author, body, file_path, line_no) {
        Ok(c) => {
            if is_api {
                json_response(json!({
                    "status": "added",
                    "comment_id": c.id,
                    "pr_id": pr_id,
                }))
            } else {
                redirect_response(&format!("/pull/{}", pr_id))
            }
        }
        Err(e) => error_response(&format!("Failed to add comment: {}", e)),
    }
}

/// Handler for `POST /api/pulls/merge`.
pub fn handle_pr_merge_post(
    body_params: &HashMap<String, String>,
    storage_dir: &Path,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
    is_api: bool,
) -> HttpResponse {
    let pr_id: usize = match body_params.get("pr_id").and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return error_response("Missing or invalid pr_id"),
    };

    let review_mgr = ReviewManager::new(storage_dir);
    match review_mgr.merge_pr(pr_id, blob_store, meta_store) {
        Ok(merge_commit) => {
            if is_api {
                json_response(json!({
                    "status": "merged",
                    "pr_id": pr_id,
                    "merge_commit": merge_commit.to_string(),
                }))
            } else {
                redirect_response(&format!("/pull/{}", pr_id))
            }
        }
        Err(e) => error_response(&format!("Merge failed: {}", e)),
    }
}
