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

//! # E2E Local-First Integration Workflow
//!
//! Validates end-to-end operation of the 100% local-first architecture:
//! 1. Embedded SQLite metadata store (`SqliteMetadataStore`) for ACID branch refs & leases.
//! 2. S3-compatible remote CAS (`S3BlobStore`) tiered with local NVMe L1 cache (`CachedBlobStore`).
//! 3. Multi-agent distributed collaboration: Agent 1 pushes to S3 + SQLite, Agent 2 starts
//!    cold and transparently hydrates blobs on-demand from S3 CAS.

use graft::cas::s3::{MockS3Transport, S3BlobStore, S3Config};
use graft::cas::{BlobStore, CachedBlobStore, Commit, LocalBlobStore};
use graft::git::{export_tree_to_dir, ingest_directory};
use graft::metadata::{MetadataStore, SqliteMetadataStore};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn test_e2e_local_first_sqlite_and_s3_tiered_cas() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = tempdir()?;
    let cas_dir_a = sandbox.path().join("agent_a_cas");
    let cas_dir_b = sandbox.path().join("agent_b_cas");
    let repo_src = sandbox.path().join("repo_source");
    let repo_b_checkout = sandbox.path().join("agent_b_checkout");
    let sqlite_db = sandbox.path().join("graft_metadata.db");

    // Setup source files
    fs::create_dir_all(repo_src.join("src").join("utils"))?;
    fs::write(
        repo_src.join("README.md"),
        b"# Local-First Graft\nZero cloud accounts needed. Pure SQLite + S3 CAS.\n",
    )?;
    fs::write(
        repo_src.join("src").join("main.rs"),
        b"fn main() { println!(\"Zero cloud cost, 100% local-first\"); }\n",
    )?;
    fs::write(
        repo_src.join("src").join("utils").join("calc.rs"),
        b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )?;

    // 1. Initialize embedded SQLite metadata store (shared by both agents)
    let meta_store = Arc::new(SqliteMetadataStore::open(&sqlite_db)?);

    // 2. Setup Tiered CAS for Agent A and Agent B backed by shared S3 object store
    let l1_a = Arc::new(LocalBlobStore::new(&cas_dir_a)?);
    let l1_b = Arc::new(LocalBlobStore::new(&cas_dir_b)?);

    let s3_config = S3Config::parse("s3://local-minio-bucket/graft-cas")?;
    let shared_s3_transport = Arc::new(MockS3Transport::new());
    let s3_store_a = Arc::new(S3BlobStore::with_transport(
        s3_config.clone(),
        shared_s3_transport.clone(),
    ));
    let s3_store_b = Arc::new(S3BlobStore::with_transport(s3_config, shared_s3_transport.clone()));

    let store_a: Arc<dyn BlobStore> = Arc::new(CachedBlobStore::new(l1_a.clone(), s3_store_a.clone()));
    let store_b: Arc<dyn BlobStore> = Arc::new(CachedBlobStore::new(l1_b.clone(), s3_store_b.clone()));

    // 3. Agent 1 ingests directory and creates initial baseline commit
    let ingest_res = ingest_directory(&repo_src, store_a.as_ref())?;
    assert_eq!(ingest_res.total_files, 3);

    let commit_1 = Commit::new(
        vec![],
        ingest_res.root_digest,
        "Graft Agent <agent@graft.dev>".to_string(),
        "Initial monorepo baseline".to_string(),
    );
    let commit_1_digest = store_a.put(&commit_1.serialize())?;

    // Record commit in SQLite
    meta_store.compare_and_swap_ref("refs/heads/main", None, &commit_1_digest, &commit_1.author)?;
    meta_store.put_commit_metadata(&commit_1_digest, &commit_1)?;

    assert_eq!(
        meta_store.get_ref("refs/heads/main")?,
        Some(commit_1_digest)
    );

    // Verify S3 received the blobs
    assert!(s3_store_a.has(&commit_1_digest));
    assert!(s3_store_a.has(&ingest_res.root_digest));

    // 4. Agent 1 acquires workspace lease on task branch
    let lease = meta_store.acquire_lease("task-agent-1", "agent-alpha", &commit_1_digest, 600)?;
    assert_eq!(lease.workspace_id, "task-agent-1");
    assert_eq!(lease.agent_id, "agent-alpha");
    assert!(lease.is_active());

    // Agent 2 cannot steal active lease
    let steal_err = meta_store
        .acquire_lease("task-agent-1", "agent-beta", &commit_1_digest, 600)
        .unwrap_err();
    assert_eq!(steal_err.kind(), std::io::ErrorKind::AlreadyExists);

    // 5. Agent 2 starts with COLD local cache, fetches main ref from SQLite and checks out repo
    assert!(!l1_b.has(&commit_1_digest)); // Cold cache!

    let main_ref = meta_store.get_ref("refs/heads/main")?.unwrap();
    let commit_bytes = store_b.get(&main_ref)?.expect("commit must exist in S3");
    let fetched_commit = Commit::deserialize(&commit_bytes)?;
    assert_eq!(fetched_commit.root_tree_digest, ingest_res.root_digest);

    // Now L1 cache of Agent B has hydrated the commit
    assert!(l1_b.has(&commit_1_digest));

    // Export files to disk (hydrates all missing files from S3 through L1)
    export_tree_to_dir(&fetched_commit.root_tree_digest, &repo_b_checkout, store_b.as_ref())?;

    let readme = fs::read_to_string(repo_b_checkout.join("README.md"))?;
    assert!(readme.contains("Zero cloud accounts needed. Pure SQLite + S3 CAS."));
    let calc = fs::read_to_string(repo_b_checkout.join("src").join("utils").join("calc.rs"))?;
    assert!(calc.contains("pub fn add"));

    // 6. Agent 2 creates a new branch and commits changes
    fs::write(
        repo_b_checkout.join("src").join("utils").join("calc.rs"),
        b"pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn mul(a: i32, b: i32) -> i32 { a * b }\n",
    )?;

    let ingest_b = ingest_directory(&repo_b_checkout, store_b.as_ref())?;
    let commit_2 = Commit::new(
        vec![commit_1_digest],
        ingest_b.root_digest,
        "Agent Beta <beta@agents.local>".to_string(),
        "Add multiplication utility".to_string(),
    );
    let commit_2_digest = store_b.put(&commit_2.serialize())?;

    meta_store.compare_and_swap_ref(
        "refs/heads/feature-mul",
        None,
        &commit_2_digest,
        &commit_2.author,
    )?;
    meta_store.put_commit_metadata(&commit_2_digest, &commit_2)?;

    // 7. Verify SQLite ref tracking & lease release
    let all_branches = meta_store.list_refs("refs/heads/")?;
    assert_eq!(all_branches.len(), 2);

    meta_store.release_lease("task-agent-1", "agent-alpha", false)?;
    assert_eq!(meta_store.get_lease("task-agent-1")?, None);

    Ok(())
}
