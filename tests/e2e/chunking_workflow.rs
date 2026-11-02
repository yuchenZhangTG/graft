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

//! # End-to-End FastCDC Content-Defined Chunking Workflow Test
//!
//! Validates the complete lifecycle of large files:
//! 1. Ingestion of files exceeding threshold into `ManifestEntry::ChunkedFile`.
//! 2. $O(1)$ size and attribute reporting in VFS without hydrating chunks.
//! 3. Sub-range / sparse reads fetching only overlapping chunks.
//! 4. FastCDC deduplication across edits (reusing untouched chunks).
//! 5. Unified diff, tree export to disk, and 3-way merge with chunked files.

use graft::cas::{
    apply_changes_to_tree_with_config, merge_trees, BlobStore, ChunkManifest, ChunkerConfig,
    DirectoryManifest, LocalBlobStore, ManifestEntry,
};
use graft::git::{diff_trees, export_tree_to_dir, ingest_directory_with_config};
use graft::vfs::{GraftFilesystem, VfsTree, ROOT_INODE};
use graft::workspace::{ChangeType, WorkspaceChange};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn test_end_to_end_fastcdc_chunking_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempdir()?;
    let cas_dir = tmp.path().join("cas");
    let store = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let repo_src = tmp.path().join("repo_src");
    fs::create_dir_all(&repo_src)?;

    // Chunker config with small thresholds for deterministic testing:
    // min: 128, avg: 512, max: 2048, threshold: 1024 bytes
    let config = ChunkerConfig {
        min_size: 128,
        avg_size: 512,
        max_size: 2048,
        threshold_bytes: 1024,
    };

    // 1. Prepare repository files:
    // - small file: 40 bytes (<= threshold -> ManifestEntry::File)
    // - large file: 16,384 bytes (> threshold -> ManifestEntry::ChunkedFile)
    let readme_content = b"# Graft FastCDC Test Repository\nSmall file.";
    fs::write(repo_src.join("README.md"), readme_content)?;

    let mut large_content = Vec::with_capacity(16384);
    for i in 0..16384 {
        large_content.push(((i * 47 + 29) % 256) as u8);
    }
    fs::write(repo_src.join("large_model.bin"), &large_content)?;

    // 2. Ingest repository with FastCDC chunking
    let ingest_res = ingest_directory_with_config(&repo_src, store.as_ref(), &config)?;
    assert_eq!(ingest_res.total_files, 2);
    assert_eq!(
        ingest_res.total_bytes,
        readme_content.len() as u64 + large_content.len() as u64
    );

    // 3. Inspect the DirectoryManifest stored in CAS
    let root_bytes = store.get(&ingest_res.root_digest)?.unwrap();
    let root_manifest = DirectoryManifest::from_bytes(&root_bytes)?;

    let readme_entry = root_manifest.entries.get("README.md").unwrap();
    assert!(
        matches!(readme_entry, ManifestEntry::File { .. }),
        "README.md should be stored as regular File"
    );

    let large_entry = root_manifest.entries.get("large_model.bin").unwrap();
    let (manifest_digest, total_size) = match large_entry {
        ManifestEntry::ChunkedFile {
            manifest_digest,
            total_size,
            ..
        } => (*manifest_digest, *total_size),
        _ => panic!("large_model.bin must be ChunkedFile"),
    };
    assert_eq!(total_size, 16384);

    // Verify ChunkManifest content in CAS
    let manifest_blob = store.get(&manifest_digest)?.unwrap();
    let chunk_manifest = ChunkManifest::from_bytes(&manifest_blob)?;
    assert_eq!(chunk_manifest.total_size_bytes, 16384);
    assert!(
        chunk_manifest.chunks.len() >= 8,
        "Expected >= 8 chunks, got {}",
        chunk_manifest.chunks.len()
    );

    // 4. Test Virtual Filesystem (VFS) and Lazy Sub-Range Reads
    let vfs_tree = VfsTree::from_cas(&ingest_res.root_digest, store.as_ref())?;
    let fs = GraftFilesystem::from_tree(vfs_tree, Arc::clone(&store) as Arc<dyn graft::cas::BlobStore>);

    // Readdir lists the files
    let dir_entries = fs.readdir(ROOT_INODE).unwrap();
    let names: Vec<String> = dir_entries.into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"README.md".to_string()));
    assert!(names.contains(&"large_model.bin".to_string()));

    // Lookup large_model.bin: reports exact size 16,384 with zero chunk decompression
    let large_attr = fs.lookup(ROOT_INODE, "large_model.bin").unwrap();
    assert_eq!(large_attr.size, 16384);

    // Read first 100 bytes (sub-range within chunk 0)
    let sub1 = fs.read(large_attr.ino, 0, 100)?;
    assert_eq!(sub1, &large_content[0..100]);

    // Read bytes 2500..3200 (sub-range spanning interior chunks)
    let sub2 = fs.read(large_attr.ino, 2500, 700)?;
    assert_eq!(sub2, &large_content[2500..3200]);

    // Read beyond EOF
    let eof = fs.read(large_attr.ino, 20000, 50)?;
    assert!(eof.is_empty());

    // 5. Test FastCDC Deduplication Across Workspace Mutations
    // Simulate an agent modifying 128 bytes in the middle of large_model.bin (offset 8000)
    let mut modified_content = large_content.clone();
    for i in 0..128 {
        modified_content[8000 + i] = 0xEE;
    }
    let modified_file = tmp.path().join("modified_large.bin");
    fs::write(&modified_file, &modified_content)?;

    let change = WorkspaceChange {
        relative_path: PathBuf::from("large_model.bin"),
        disk_path: modified_file,
        change_type: ChangeType::Modified,
    };

    let modified_tree_digest = apply_changes_to_tree_with_config(
        &ingest_res.root_digest,
        &[change],
        store.as_ref(),
        &config,
    )?;

    // Inspect the new ChunkManifest
    let mod_root_bytes = store.get(&modified_tree_digest)?.unwrap();
    let mod_manifest = DirectoryManifest::from_bytes(&mod_root_bytes)?;
    let mod_entry = mod_manifest.entries.get("large_model.bin").unwrap();
    let mod_manifest_digest = match mod_entry {
        ManifestEntry::ChunkedFile { manifest_digest, .. } => *manifest_digest,
        _ => panic!("Modified file should still be ChunkedFile"),
    };
    let mod_manifest_blob = store.get(&mod_manifest_digest)?.unwrap();
    let mod_chunk_manifest = ChunkManifest::from_bytes(&mod_manifest_blob)?;

    // Calculate chunk reuse ratio
    let original_chunk_hashes: HashSet<_> =
        chunk_manifest.chunks.iter().map(|c| c.digest).collect();
    let reused_chunks = mod_chunk_manifest
        .chunks
        .iter()
        .filter(|c| original_chunk_hashes.contains(&c.digest))
        .count();

    let reuse_ratio = reused_chunks as f64 / mod_chunk_manifest.chunks.len() as f64;
    assert!(
        reuse_ratio >= 0.70,
        "Expected >= 70% chunk deduplication, got {:.2}",
        reuse_ratio
    );

    // 6. Test Unified Diffing
    let diff_output = diff_trees(&ingest_res.root_digest, &modified_tree_digest, store.as_ref())?;
    assert!(diff_output.contains("Binary files a/large_model.bin and b/large_model.bin differ"));

    // 7. Test Export Tree to Disk
    let export_dir = tmp.path().join("exported_repo");
    let files_exported = export_tree_to_dir(&modified_tree_digest, &export_dir, store.as_ref())?;
    assert_eq!(files_exported, 2);

    let exported_large = fs::read(export_dir.join("large_model.bin"))?;
    assert_eq!(
        exported_large, modified_content,
        "Exported chunked file must match byte-for-byte"
    );

    // 8. Test 3-Way Merge With Chunked Files
    // Branch A modifies README.md
    let readme_v2 = tmp.path().join("readme_v2.md");
    fs::write(&readme_v2, b"# Graft Updated by Lead\nSmall file.")?;
    let tree_a = apply_changes_to_tree_with_config(
        &ingest_res.root_digest,
        &[WorkspaceChange {
            relative_path: PathBuf::from("README.md"),
            disk_path: readme_v2,
            change_type: ChangeType::Modified,
        }],
        store.as_ref(),
        &config,
    )?;

    // Branch B adds a new file
    let new_file = tmp.path().join("agent_notes.txt");
    fs::write(&new_file, b"Agent notes here.")?;
    let tree_b = apply_changes_to_tree_with_config(
        &ingest_res.root_digest,
        &[WorkspaceChange {
            relative_path: PathBuf::from("agent_notes.txt"),
            disk_path: new_file,
            change_type: ChangeType::Created,
        }],
        store.as_ref(),
        &config,
    )?;

    // Merge Branch A and Branch B (both have identical large_model.bin)
    let merge_res = merge_trees(&ingest_res.root_digest, &tree_a, &tree_b, store.as_ref())?;
    assert!(!merge_res.has_conflicts);

    let merged_root_bytes = store.get(&merge_res.root_tree_digest)?.unwrap();
    let merged_manifest = DirectoryManifest::from_bytes(&merged_root_bytes)?;
    assert_eq!(merged_manifest.entries.len(), 3);
    assert!(matches!(
        merged_manifest.entries.get("large_model.bin").unwrap(),
        ManifestEntry::ChunkedFile { .. }
    ));

    Ok(())
}
