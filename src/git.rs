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

//! # Git Interoperability Subsystem
//!
//! Provides bridges between Graft's internal Merkle tree and the wider Git ecosystem:
//! - **Ingestion**: Importing external directories and Git tree objects into CAS (`ingest`).
//! - **Unified Diffing**: Computing diffs between commits and workspaces (`diff`).
//! - **Export**: Materializing Merkle trees to disk and exporting patches (`export`).

pub mod blame;
pub mod diff;
pub mod export;
pub mod ingest;

pub use blame::{compute_blame, BlameEntry, BlameResult};
pub use diff::{
    compute_diff_stats, compute_file_diff_ast, compute_intraline_diff, diff_trees,
    diff_trees_report, diff_workspace, diff_workspace_report, generate_unified_diff, DiffHunk,
    DiffLine, DiffReport, DiffStats, FileDiff, FileDiffStatus, SplitCell, SplitCellType, SplitRow,
    UnifiedRow,
};
pub use export::{export_patch_file, export_tree_to_dir};
pub use ingest::{
    ingest_directory, ingest_directory_with_config, parse_git_tree_entries, GitTreeEntry,
    IngestResult,
};
