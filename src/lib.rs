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

//! # Graft: High-Performance Virtualized Source Control for AI Agents & Monorepos
//!
//! Graft provides instant O(1) workspace checkouts, lazy file hydration from Content-Addressed
//! Storage (CAS), and O(K) delta commit extraction for high-velocity autonomous coding agents.

pub mod cas;
pub mod config;
pub mod git;
pub mod metadata;
pub mod review;
pub mod vfs;
pub mod web;
pub mod workspace;

pub use review::{PullRequest, PullRequestStatus, ReviewComment, ReviewManager};

pub use config::GraftConfigFile;

pub use cas::{
    apply_changes_to_tree, apply_changes_to_tree_with_config, apply_tree_delta_to_workspace,
    cherry_pick_commit, chunk_file, chunk_slice, collect_roots, extract_tree_delta,
    find_common_ancestor, get_commit_history, mark_reachable, merge3_text,
    merge3_text_with_strategy, merge_trees, merge_trees_with_strategy, read_chunked_range,
    run_gc, store_file_or_chunked, stream_chunked_file, BlobInfo, BlobStore, CachedBlobStore,
    CherryPickResult, ChunkEntry, ChunkManifest, ChunkerConfig, Commit, ConflictStrategy, Digest,
    DirectoryManifest, GcOptions, GcStats, GcsBlobStore, LocalBlobStore, ManifestEntry,
    MergeResult, TreeDelta,
};
pub use git::{
    diff_trees, diff_workspace, export_patch_file, export_tree_to_dir, generate_unified_diff,
    ingest_directory, ingest_directory_with_config, parse_git_tree_entries, GitTreeEntry,
    IngestResult,
};
pub use metadata::{
    normalize_ref_name, parse_rfc3339_utc, resolve_ref, validate_branch_name, LocalMetadataStore,
    MetadataStore, SpannerConfig, SpannerMetadataStore, WorkspaceLease,
};
pub use vfs::{DirEntry, FileAttr, FileType, GraftFilesystem, Inode, VfsNode, VfsTree, ROOT_INODE};
#[cfg(feature = "fuse")]
pub use vfs::{default_mount_options, mount_fuse, spawn_mount_fuse};
pub use web::{
    route_request, search_cas_tree, start_web_server, SearchFileResult, SearchResponse,
    SearchSnippet, WebConfig,
};
pub use workspace::{
    ChangeType, CherryPickState, WorkspaceChange, WorkspaceConfig, WorkspaceManager,
};
