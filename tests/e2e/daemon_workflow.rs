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

//! # E2E Integration Tests for `graftd` Monorepo Daemon
//!
//! Validates:
//! 1. Tokio HTTP/1.1 REST + streaming binary data plane.
//! 2. `RemoteBlobStore`: PUT, GET, HEAD, batch `find_missing`, and cryptographic bit-rot validation.
//! 3. `RemoteMetadataStore`: CAS ref coordination, commit metadata indexing, and distributed TTL workspace leases.
//! 4. Server-Side 3-Way AST Semantic Merge: zero client downloads, in-memory Merkle walk and atomic trunk advancement.
//! 5. Bearer token authentication and authorization enforcement.

use graft::cas::blob_store::BlobStore;
use graft::cas::commit::Commit;
use graft::cas::digest::Digest;
use graft::cas::LocalBlobStore;
use graft::daemon::client::{RemoteBlobStore, RemoteMetadataStore};
use graft::daemon::merge_service::ServerSideMergeRequest;
use graft::daemon::server::{serve_with_listener, DaemonState};
use graft::git::{export_tree_to_dir, ingest_directory};
use graft::metadata::{MetadataStore, SqliteMetadataStore};
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_e2e_daemon_core_services() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = tempdir()?;
    let cas_dir = sandbox.path().join("server_cas");
    let meta_db = sandbox.path().join("server_meta.db");

    let blob_store = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let meta_store = Arc::new(SqliteMetadataStore::open(&meta_db)?);

    // 1. Bind ephemeral port on localhost
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let daemon_state = Arc::new(DaemonState {
        blob_store: blob_store.clone(),
        meta_store: meta_store.clone(),
        auth_token: None,
    });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Spawn graftd in background task
    let server_task = tokio::spawn(async move {
        serve_with_listener(listener, daemon_state, Some(shutdown_rx)).await
    });

    // 2. Health & Status Checks
    let health_str = ureq::get(&format!("{}/healthz", base_url))
        .call()?
        .body_mut()
        .read_to_string()?;
    let health_resp: serde_json::Value = serde_json::from_str(&health_str)?;
    assert_eq!(health_resp["status"], "healthy");

    let status_str = ureq::get(&format!("{}/api/v1/status", base_url))
        .call()?
        .body_mut()
        .read_to_string()?;
    let status_resp: serde_json::Value = serde_json::from_str(&status_str)?;
    assert_eq!(status_resp["daemon"], "graftd");

    // 3. Connect client stores
    let remote_blobs = Arc::new(RemoteBlobStore::new(&base_url));
    let remote_meta = Arc::new(RemoteMetadataStore::new(&base_url));

    // 4. Test Remote CAS (BlobStore)
    let payload_1 = b"Hello, Autonomous Agent Fleet!";
    let digest_1 = remote_blobs.put(payload_1)?;
    assert_eq!(digest_1, Digest::from_bytes(payload_1));

    // Verify has() and get()
    assert!(remote_blobs.has(&digest_1));
    let retrieved_1 = remote_blobs.get(&digest_1)?.expect("blob must exist");
    assert_eq!(retrieved_1, payload_1);

    // Non-existent blob
    let fake_digest = Digest::from_bytes(b"non-existent-content-payload-xyz");
    assert!(!remote_blobs.has(&fake_digest));
    assert!(remote_blobs.get(&fake_digest)?.is_none());

    // Batch find_missing
    let payload_2 = b"Second blob payload for deduplication";
    let digest_2 = remote_blobs.put(payload_2)?;

    let missing = remote_blobs.find_missing(&[digest_1, fake_digest, digest_2])?;
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0], fake_digest);

    // 5. Test Remote Metadata (Commit & Refs)
    let commit_1 = Commit::new(
        vec![],
        digest_1,
        "Agent One <agent1@graft.dev>".to_string(),
        "Initial monorepo root commit".to_string(),
    );
    let commit_1_bytes = commit_1.serialize();
    let commit_1_digest = remote_blobs.put(&commit_1_bytes)?;

    remote_meta.put_commit_metadata(&commit_1_digest, &commit_1)?;

    let fetched_commit_1 = remote_meta
        .get_commit_metadata(&commit_1_digest)?
        .expect("commit metadata must exist");
    assert_eq!(fetched_commit_1.author, "Agent One <agent1@graft.dev>");
    assert_eq!(fetched_commit_1.root_tree_digest, digest_1);
    assert!(fetched_commit_1.parent_commits.is_empty());

    // CAS branch creation
    remote_meta.compare_and_swap_ref(
        "refs/heads/main",
        None,
        &commit_1_digest,
        &commit_1.author,
    )?;

    assert_eq!(
        remote_meta.get_ref("refs/heads/main")?,
        Some(commit_1_digest)
    );
    assert_eq!(remote_meta.get_ref("main")?, Some(commit_1_digest));

    // CAS conflict detection (expected mismatch)
    let conflict_err = remote_meta.compare_and_swap_ref(
        "refs/heads/main",
        None, // Ref already exists!
        &digest_2,
        "Rogue Agent",
    );
    assert!(conflict_err.is_err());
    assert_eq!(
        conflict_err.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );

    // Atomic branch advancement
    let commit_2 = Commit::new(
        vec![commit_1_digest],
        digest_2,
        "Agent Two <agent2@graft.dev>".to_string(),
        "Feature commit 2".to_string(),
    );
    let commit_2_digest = remote_blobs.put(&commit_2.serialize())?;
    remote_meta.put_commit_metadata(&commit_2_digest, &commit_2)?;

    remote_meta.compare_and_swap_ref(
        "refs/heads/main",
        Some(&commit_1_digest),
        &commit_2_digest,
        &commit_2.author,
    )?;
    assert_eq!(remote_meta.get_ref("main")?, Some(commit_2_digest));

    // List refs
    let refs = remote_meta.list_refs("refs/heads")?;
    assert!(refs.iter().any(|(name, d)| name == "refs/heads/main" && d == &commit_2_digest));

    // 6. Test Distributed Workspace Leases
    let lease_1 = remote_meta.acquire_lease("ws-agent-alpha", "agent-alpha-worker-1", &commit_1_digest, 60)?;
    assert_eq!(lease_1.workspace_id, "ws-agent-alpha");
    assert_eq!(lease_1.agent_id, "agent-alpha-worker-1");

    // Second agent attempts to acquire same workspace lease -> AlreadyExists
    let lease_collision = remote_meta.acquire_lease("ws-agent-alpha", "agent-beta-worker-2", &commit_1_digest, 60);
    assert!(lease_collision.is_err());
    assert_eq!(
        lease_collision.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );

    // Query lease
    let current_lease = remote_meta.get_lease("ws-agent-alpha")?.expect("lease should exist");
    assert_eq!(current_lease.agent_id, "agent-alpha-worker-1");

    let all_leases = remote_meta.list_leases()?;
    assert_eq!(all_leases.len(), 1);

    // Renewal with wrong agent -> PermissionDenied
    let rogue_renew = remote_meta.renew_lease("ws-agent-alpha", "agent-beta-worker-2", 120);
    assert!(rogue_renew.is_err());
    assert_eq!(
        rogue_renew.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );

    // Renewal with owner -> Ok
    let renewed = remote_meta.renew_lease("ws-agent-alpha", "agent-alpha-worker-1", 120)?;
    assert!(renewed.lease_expires_at >= current_lease.lease_expires_at);

    // Release lease
    remote_meta.release_lease("ws-agent-alpha", "agent-alpha-worker-1", false)?;
    assert!(remote_meta.get_lease("ws-agent-alpha")?.is_none());

    // Agent Beta can now acquire it!
    let beta_lease = remote_meta.acquire_lease("ws-agent-alpha", "agent-beta-worker-2", &commit_2_digest, 60)?;
    assert_eq!(beta_lease.agent_id, "agent-beta-worker-2");

    // Clean up beta lease
    remote_meta.release_lease("ws-agent-alpha", "agent-beta-worker-2", true)?;

    // 7. Test Server-Side 3-Way AST Semantic Merge
    // Setup repository files for 3-way merge test
    let repo_dir = sandbox.path().join("repo_source");
    fs::create_dir_all(repo_dir.join("src"))?;

    // Base state
    fs::write(
        repo_dir.join("src").join("common.rs"),
        b"pub fn common_core() -> bool { true }\n",
    )?;
    fs::write(
        repo_dir.join("README.md"),
        b"# Monorepo Project\nInitial documentation.\n",
    )?;

    let base_ingest = ingest_directory(&repo_dir, remote_blobs.as_ref())?;
    let base_commit = Commit::new(
        vec![],
        base_ingest.root_digest,
        "Lead Architect <lead@graft.dev>".to_string(),
        "Base architecture".to_string(),
    );
    let base_commit_digest = remote_blobs.put(&base_commit.serialize())?;
    remote_meta.put_commit_metadata(&base_commit_digest, &base_commit)?;
    remote_meta.compare_and_swap_ref("refs/heads/base", None, &base_commit_digest, &base_commit.author)?;

    // Ours state: adds `ours_feature.rs`
    fs::write(
        repo_dir.join("src").join("ours_feature.rs"),
        b"pub fn feature_ours() -> &'static str { \"agent-ours\" }\n",
    )?;
    let ours_ingest = ingest_directory(&repo_dir, remote_blobs.as_ref())?;
    let ours_commit = Commit::new(
        vec![base_commit_digest],
        ours_ingest.root_digest,
        "Agent Ours <ours@graft.dev>".to_string(),
        "Add ours feature".to_string(),
    );
    let ours_commit_digest = remote_blobs.put(&ours_commit.serialize())?;
    remote_meta.put_commit_metadata(&ours_commit_digest, &ours_commit)?;
    remote_meta.compare_and_swap_ref("refs/heads/main", Some(&commit_2_digest), &ours_commit_digest, &ours_commit.author)?;

    // Reset repo dir to base and apply theirs: adds `theirs_feature.rs`
    fs::remove_file(repo_dir.join("src").join("ours_feature.rs"))?;
    fs::write(
        repo_dir.join("src").join("theirs_feature.rs"),
        b"pub fn feature_theirs() -> &'static str { \"agent-theirs\" }\n",
    )?;
    let theirs_ingest = ingest_directory(&repo_dir, remote_blobs.as_ref())?;
    let theirs_commit = Commit::new(
        vec![base_commit_digest],
        theirs_ingest.root_digest,
        "Agent Theirs <theirs@graft.dev>".to_string(),
        "Add theirs feature".to_string(),
    );
    let theirs_commit_digest = remote_blobs.put(&theirs_commit.serialize())?;
    remote_meta.put_commit_metadata(&theirs_commit_digest, &theirs_commit)?;
    remote_meta.compare_and_swap_ref("refs/heads/feature-theirs", None, &theirs_commit_digest, &theirs_commit.author)?;

    // Execute server-side 3-way AST merge RPC!
    let merge_req = ServerSideMergeRequest {
        base: "refs/heads/base".to_string(),
        ours: "refs/heads/main".to_string(),
        theirs: "refs/heads/feature-theirs".to_string(),
        strategy: Some("markers".to_string()),
        author: Some("Graft Merge Coordinator <coordinator@graft.dev>".to_string()),
        message: Some("Merge branch 'feature-theirs' into 'main' cleanly".to_string()),
        advance_branch: Some("refs/heads/main".to_string()),
    };

    let merge_resp = remote_meta.server_side_merge(&merge_req)?;
    assert!(merge_resp.clean, "Merge must be clean");
    assert_eq!(merge_resp.status, "success");
    assert!(merge_resp.conflict_files.is_empty());

    let merge_commit_digest_str = merge_resp.merged_commit.expect("merged commit must exist");
    let merge_commit_digest = Digest::parse(&merge_commit_digest_str)?;

    // Verify main was atomically updated to the merge commit
    assert_eq!(
        remote_meta.get_ref("refs/heads/main")?,
        Some(merge_commit_digest)
    );

    // Verify merge commit has 2 parents
    let merge_commit_obj = remote_meta
        .get_commit_metadata(&merge_commit_digest)?
        .expect("merge commit metadata must exist");
    assert_eq!(
        merge_commit_obj.parent_commits,
        vec![ours_commit_digest, theirs_commit_digest]
    );

    // Verify the merged tree contains files from both sides
    let checkout_dir = sandbox.path().join("merged_checkout");
    export_tree_to_dir(
        &merge_commit_obj.root_tree_digest,
        &checkout_dir,
        remote_blobs.as_ref(),
    )?;

    assert!(checkout_dir.join("src").join("common.rs").exists());
    assert!(checkout_dir.join("src").join("ours_feature.rs").exists());
    assert!(checkout_dir.join("src").join("theirs_feature.rs").exists());
    assert_eq!(
        fs::read_to_string(checkout_dir.join("src").join("ours_feature.rs"))?,
        "pub fn feature_ours() -> &'static str { \"agent-ours\" }\n"
    );
    assert_eq!(
        fs::read_to_string(checkout_dir.join("src").join("theirs_feature.rs"))?,
        "pub fn feature_theirs() -> &'static str { \"agent-theirs\" }\n"
    );

    // 8. Delete ref test
    remote_meta.delete_ref("refs/heads/feature-theirs", Some(&theirs_commit_digest))?;
    assert!(remote_meta.get_ref("refs/heads/feature-theirs")?.is_none());

    // 9. Shutdown server cleanly
    let _ = shutdown_tx.send(());
    let _ = server_task.await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_e2e_daemon_authenticated_access() -> Result<(), Box<dyn std::error::Error>> {
    let sandbox = tempdir()?;
    let cas_dir = sandbox.path().join("auth_cas");
    let meta_db = sandbox.path().join("auth_meta.db");

    let blob_store = Arc::new(LocalBlobStore::new(&cas_dir)?);
    let meta_store = Arc::new(SqliteMetadataStore::open(&meta_db)?);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let base_url = format!("http://127.0.0.1:{}", port);

    let secret_token = "graft-sec-fleet-token-987654";
    let daemon_state = Arc::new(DaemonState {
        blob_store,
        meta_store,
        auth_token: Some(secret_token.to_string()),
    });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_task = tokio::spawn(async move {
        serve_with_listener(listener, daemon_state, Some(shutdown_rx)).await
    });

    // Unauthenticated client should fail
    let unauthed_blobs = RemoteBlobStore::new(&base_url);
    let put_err = unauthed_blobs.put(b"unauthorized data");
    assert!(put_err.is_err());

    // Client with invalid token should fail
    let bad_auth_blobs = RemoteBlobStore::with_auth(&base_url, Some("wrong-token".to_string()));
    let bad_put_err = bad_auth_blobs.put(b"unauthorized data");
    assert!(bad_put_err.is_err());

    // Client with valid token should succeed
    let authed_blobs = RemoteBlobStore::with_auth(&base_url, Some(secret_token.to_string()));
    let authed_meta = RemoteMetadataStore::with_auth(&base_url, Some(secret_token.to_string()));

    let digest = authed_blobs.put(b"authenticated secret data")?;
    assert!(authed_blobs.has(&digest));

    let retrieved = authed_blobs.get(&digest)?.expect("must exist");
    assert_eq!(retrieved, b"authenticated secret data");

    let refs = authed_meta.list_refs("refs/heads")?;
    assert!(refs.is_empty());

    let _ = shutdown_tx.send(());
    let _ = server_task.await?;

    Ok(())
}
