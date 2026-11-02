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

//! # End-to-End Distributed GCS Multi-Agent Workflow Test
//!
//! Validates the full distributed agent workflow across GCP Cloud Storage (`gs://`):
//! 1. Agent 1 on Machine A has a local NVMe L1 cache (`cas_a`) backed by GCS L2.
//!    - Ingests repository, produces root Merkle digest, and records Commit A.
//!    - Verifies blobs are written to local cache and written through to GCS.
//! 2. Agent 2 on Machine B has an independent, empty local cache (`cas_b`) backed by the same GCS L2.
//!    - Starts with a completely cold L1 cache.
//!    - Requests Commit A: triggers transparent lazy hydration from GCS into `cas_b`.
//!    - Materializes the Merkle tree onto disk; all missing blobs stream from GCS on demand.
//!    - Verifies content integrity and bit-for-bit accuracy.
//! 3. Supports live cloud testing against `gs://my-bucket/graft-test` when `GRAFT_TEST_LIVE_GCS=1`.

use graft::cas::{
    BlobStore, CachedBlobStore, Commit, GcsBlobStore, GcsConfig, LocalBlobStore, MockGcsTransport,
};
use graft::git::{export_tree_to_dir, ingest_directory};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn test_end_to_end_distributed_gcs_workflow() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = tempdir()?;
    let cas_dir_a = sandbox.path().join("agent_a_cas");
    let cas_dir_b = sandbox.path().join("agent_b_cas");
    let repo_src = sandbox.path().join("repo_source");
    let repo_b_out = sandbox.path().join("agent_b_checkout");

    // Setup source repository for Agent A
    fs::create_dir_all(repo_src.join("src").join("models"))?;
    fs::write(
        repo_src.join("README.md"),
        b"# Graft Monorepo on GCP Cloud Storage\nDistributed multi-agent source control.\n",
    )?;
    fs::write(
        repo_src.join("src").join("main.rs"),
        b"fn main() { println!(\"Running on Graft + GCS\"); }\n",
    )?;
    fs::write(
        repo_src.join("src").join("models").join("agent.rs"),
        b"pub struct AgentState { pub id: String, pub ready: bool }\n",
    )?;

    // Setup Storage Backends
    let l1_a = Arc::new(LocalBlobStore::new(&cas_dir_a)?);
    let l1_b = Arc::new(LocalBlobStore::new(&cas_dir_b)?);

    let (store_a, store_b): (Arc<dyn BlobStore>, Arc<dyn BlobStore>) =
        if std::env::var("GRAFT_TEST_LIVE_GCS").is_ok() {
            println!("Running against LIVE GCP bucket: gs://my-bucket/graft-test");
            let gcs_a = Arc::new(GcsBlobStore::new("gs://my-bucket/graft-test")?);
            let gcs_b = Arc::new(GcsBlobStore::new("gs://my-bucket/graft-test")?);
            (
                Arc::new(CachedBlobStore::new(l1_a.clone(), gcs_a)),
                Arc::new(CachedBlobStore::new(l1_b.clone(), gcs_b)),
            )
        } else {
            // Hermetic mock transport for fast offline CI testing
            let config = GcsConfig::parse("gs://mock-bucket/graft-test").unwrap();
            let shared_transport = Arc::new(MockGcsTransport::new());
            let gcs_a = Arc::new(GcsBlobStore::with_transport(config.clone(), shared_transport.clone()));
            let gcs_b = Arc::new(GcsBlobStore::with_transport(config, shared_transport));
            (
                Arc::new(CachedBlobStore::new(l1_a.clone(), gcs_a)),
                Arc::new(CachedBlobStore::new(l1_b.clone(), gcs_b)),
            )
        };

    // -------------------------------------------------------------------------
    // STEP 1: Agent 1 ingests repository into L1 cache and pushes to GCS
    // -------------------------------------------------------------------------
    println!("Step 1: Agent 1 ingesting repository into tiered CAS...");
    let ingest_res = ingest_directory(&repo_src, store_a.as_ref())?;
    assert_eq!(ingest_res.total_files, 3);

    let commit_a = Commit::new(
        vec![],
        ingest_res.root_digest,
        "Agent 1 <agent1@graft.dev>".to_string(),
        "Initial monorepo baseline".to_string(),
    );
    let commit_a_digest = store_a.put(&commit_a.serialize())?;

    // Verify Agent A has blobs cached locally
    assert!(l1_a.has(&commit_a_digest));
    assert!(l1_a.has(&ingest_res.root_digest));

    // -------------------------------------------------------------------------
    // STEP 2: Agent 2 starts with cold cache; verifies lazy hydration from GCS
    // -------------------------------------------------------------------------
    println!("Step 2: Agent 2 with cold cache querying commit from GCS...");
    // Agent B local cache MUST be cold initially
    assert!(!l1_b.has(&commit_a_digest));
    assert!(!l1_b.has(&ingest_res.root_digest));

    // Agent B reads commit: triggers on-demand hydration from GCS
    let commit_b_bytes = store_b
        .get(&commit_a_digest)?
        .expect("Commit must be lazily fetched from GCS");
    let commit_b = Commit::deserialize(&commit_b_bytes)?;
    assert_eq!(commit_b.root_tree_digest, ingest_res.root_digest);

    // Verify Agent B local cache is now populated with the commit object
    assert!(l1_b.has(&commit_a_digest));
    assert!(l1_b.get_file_path(&commit_a_digest).is_some());

    // -------------------------------------------------------------------------
    // STEP 3: Agent 2 materializes full tree from GCS
    // -------------------------------------------------------------------------
    println!("Step 3: Agent 2 materializing tree from GCS into local workspace...");
    let files_materialized = export_tree_to_dir(&commit_b.root_tree_digest, &repo_b_out, store_b.as_ref())?;
    assert_eq!(files_materialized, 3);

    // Verify materialized files on disk match the original
    let readme_b = fs::read(repo_b_out.join("README.md"))?;
    assert_eq!(readme_b, fs::read(repo_src.join("README.md"))?);

    let main_b = fs::read(repo_b_out.join("src").join("main.rs"))?;
    assert_eq!(main_b, fs::read(repo_src.join("src").join("main.rs"))?);

    let model_b = fs::read(repo_b_out.join("src").join("models").join("agent.rs"))?;
    assert_eq!(model_b, fs::read(repo_src.join("src").join("models").join("agent.rs"))?);

    // -------------------------------------------------------------------------
    // STEP 4: Verify zero-copy local path hydration for VFS / FUSE
    // -------------------------------------------------------------------------
    println!("Step 4: Verifying on-demand POSIX path hydration for FUSE/VFS...");
    let readme_digest = graft::cas::Digest::from_bytes(&readme_b);
    let hydrated_path = store_b
        .get_file_path(&readme_digest)
        .expect("get_file_path must return valid local path after hydration");
    assert!(hydrated_path.exists());

    println!("✓ Distributed GCS multi-agent workflow test succeeded!");
    Ok(())
}
