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

//! # Content-Addressed Storage Subsystem

pub mod blob_store;
pub mod cached_store;
pub mod chunker;
pub mod commit;
pub mod digest;
pub mod gc;
pub mod gcs;
pub mod merge;
pub mod s3;
pub mod semantic_merge;

pub use blob_store::{BlobInfo, BlobStore, LocalBlobStore};
pub use cached_store::CachedBlobStore;
pub use chunker::{
    chunk_file, chunk_slice, read_chunked_range, store_file_or_chunked, stream_chunked_file,
    ChunkEntry, ChunkManifest, ChunkerConfig,
};
pub use commit::{
    apply_changes_to_tree, apply_changes_to_tree_with_config, get_commit_history,
    get_commit_tree_digest, Commit, DirectoryManifest, ManifestEntry,
};
pub use digest::Digest;
pub use gc::{collect_roots, mark_reachable, run_gc, GcOptions, GcStats};
pub use gcs::{GcsBlobStore, GcsConfig, GcsTokenProvider, GcsTransport, MockGcsTransport, UreqGcsTransport};
pub use merge::{
    apply_tree_delta_to_workspace, cherry_pick_commit, extract_tree_delta, find_common_ancestor,
    merge3_text, merge3_text_with_strategy, merge_trees, merge_trees_with_strategy,
    CherryPickResult, ConflictStrategy, MergeResult, TreeDelta,
};
pub use s3::{MockS3Transport, S3BlobStore, S3Config, S3Transport, UreqS3Transport};
pub use semantic_merge::{semantic_merge_with_strategy, SupportedLanguage};
