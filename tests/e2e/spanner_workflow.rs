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

//! # Spanner Metadata Workflow E2E Tests
//!
//! Verifies distributed linearizable ACID branch management, multi-agent CAS contention,
//! and commit indexing over Cloud Spanner.

use graft::cas::commit::Commit;
use graft::cas::digest::Digest;
use graft::metadata::spanner::{MockSpannerClient, SpannerConfig, SpannerMetadataStore};
use graft::metadata::MetadataStore;
use std::env;
use std::sync::Arc;

#[test]
fn test_mock_spanner_multi_agent_cas_contention() {
    let mock_client = Arc::new(MockSpannerClient::new());
    let config = SpannerConfig::new("projects/test/instances/test/databases/test").unwrap();
    let store1 = SpannerMetadataStore::new(config.clone(), mock_client.clone());
    let store2 = SpannerMetadataStore::new(config, mock_client);

    let initial_digest = Digest::from_bytes(b"initial-commit");
    let agent1_digest = Digest::from_bytes(b"agent1-commit");
    let agent2_digest = Digest::from_bytes(b"agent2-commit");

    // Initialize branch
    store1
        .compare_and_swap_ref("refs/heads/main", None, &initial_digest, "admin")
        .expect("Initialize branch should succeed");

    assert_eq!(
        store2.get_ref("refs/heads/main").unwrap().unwrap(),
        initial_digest
    );

    // Agent 1 CAS from initial -> agent1
    store1
        .compare_and_swap_ref(
            "refs/heads/main",
            Some(&initial_digest),
            &agent1_digest,
            "agent-1",
        )
        .expect("Agent 1 CAS should succeed");

    // Agent 2 attempts stale CAS from initial -> agent2 (must fail!)
    let agent2_res = store2.compare_and_swap_ref(
        "refs/heads/main",
        Some(&initial_digest),
        &agent2_digest,
        "agent-2",
    );
    assert!(
        agent2_res.is_err(),
        "Agent 2 stale CAS should abort due to linearizability check"
    );

    // Agent 2 inspects current head and CAS succeeds with new head
    let current_head = store2.get_ref("refs/heads/main").unwrap().unwrap();
    assert_eq!(current_head, agent1_digest);

    store2
        .compare_and_swap_ref(
            "refs/heads/main",
            Some(&current_head),
            &agent2_digest,
            "agent-2",
        )
        .expect("Agent 2 retry with latest head should succeed");

    assert_eq!(
        store1.get_ref("refs/heads/main").unwrap().unwrap(),
        agent2_digest
    );
}

#[test]
fn test_live_spanner_cloud_crud_workflow() {
    let live_enabled = env::var("GRAFT_TEST_LIVE_SPANNER").unwrap_or_default();
    if live_enabled != "1" && live_enabled.to_lowercase() != "true" {
        eprintln!("Skipping live Spanner test. Set GRAFT_TEST_LIVE_SPANNER=1 to enable.");
        return;
    }

    let database_path = env::var("GRAFT_SPANNER_DATABASE").unwrap_or_else(|_| {
        "projects/vmm-net-prod/instances/graft-instance/databases/graft-metadata".to_string()
    });

    let store = SpannerMetadataStore::connect_gcp(&database_path)
        .expect("Failed to connect to live Spanner database");

    let test_branch = format!("refs/heads/test-live-{}", std::process::id());
    let commit1_digest = Digest::from_bytes(b"live-spanner-commit-1");
    let commit2_digest = Digest::from_bytes(b"live-spanner-commit-2");

    // 1. Create reference
    store
        .compare_and_swap_ref(&test_branch, None, &commit1_digest, "tester")
        .expect("Live Spanner ref creation failed");

    // 2. Read back reference
    let head = store
        .get_ref(&test_branch)
        .expect("Live Spanner get_ref failed")
        .expect("Reference should exist");
    assert_eq!(head.hash, commit1_digest.hash);

    // 3. Stale CAS should fail
    let stale_attempt = store.compare_and_swap_ref(
        &test_branch,
        Some(&commit2_digest),
        &commit2_digest,
        "tester",
    );
    assert!(stale_attempt.is_err());

    // 4. Correct CAS should succeed
    store
        .compare_and_swap_ref(
            &test_branch,
            Some(&commit1_digest),
            &commit2_digest,
            "tester",
        )
        .expect("Live Spanner CAS update failed");

    let head_after = store.get_ref(&test_branch).unwrap().unwrap();
    assert_eq!(head_after.hash, commit2_digest.hash);

    // 5. Store and read commit metadata
    let commit = Commit::new(
        vec![commit1_digest.clone()],
        Digest::from_bytes(b"tree-live"),
        "tester@google.com".to_string(),
        "Live Spanner test commit".to_string(),
    );
    store
        .put_commit_metadata(&commit2_digest, &commit)
        .expect("Put commit metadata failed");

    let retrieved = store
        .get_commit_metadata(&commit2_digest)
        .expect("Get commit metadata failed")
        .expect("Commit metadata should exist");
    assert_eq!(retrieved.author, "tester@google.com");
    assert_eq!(retrieved.message, "Live Spanner test commit");
    assert_eq!(retrieved.parent_commits.len(), 1);
    assert_eq!(retrieved.parent_commits[0].hash, commit1_digest.hash);

    // 6. Delete reference (clean up)
    store
        .delete_ref(&test_branch, Some(&commit2_digest))
        .expect("Live Spanner delete_ref failed");

    let final_check = store.get_ref(&test_branch).unwrap();
    assert!(final_check.is_none(), "Branch should be deleted");

    // 7. Live Distributed Lease Workflow
    let test_ws = format!("live-ws-test-{}", std::process::id());
    let lease1 = store
        .acquire_lease(&test_ws, "live-agent-1", &commit1_digest, 120)
        .expect("Live lease acquisition should succeed");
    assert_eq!(lease1.workspace_id, test_ws);
    assert_eq!(lease1.agent_id, "live-agent-1");
    assert!(lease1.is_active());

    // Competing agent must fail
    let conflict = store.acquire_lease(&test_ws, "live-agent-2", &commit1_digest, 120);
    assert!(conflict.is_err(), "Competing lease acquire must fail");

    // Heartbeat
    let renewed = store
        .renew_lease(&test_ws, "live-agent-1", 180)
        .expect("Live lease renewal should succeed");
    assert!(renewed.is_active());

    // Release lease
    store
        .release_lease(&test_ws, "live-agent-1", false)
        .expect("Live lease release should succeed");
    assert!(store.get_lease(&test_ws).unwrap().is_none(), "Lease should be released");
}

#[test]
fn test_mock_spanner_distributed_workspace_leases() {
    let mock_client = Arc::new(MockSpannerClient::new());
    let config = SpannerConfig::new("projects/test/instances/test/databases/test").unwrap();
    let store1 = SpannerMetadataStore::new(config.clone(), mock_client.clone());
    let store2 = SpannerMetadataStore::new(config, mock_client);

    let base = Digest::from_bytes(b"base-tree-0");

    // 1. Agent 1 acquires 60s lease
    let l1 = store1
        .acquire_lease("task-42", "agent-1", &base, 60)
        .expect("Agent 1 acquire lease");
    assert_eq!(l1.agent_id, "agent-1");
    assert!(l1.is_active());

    // 2. Agent 2 attempts acquisition -> conflict
    let err = store2.acquire_lease("task-42", "agent-2", &base, 60);
    assert!(err.is_err(), "Concurrent lease acquisition must fail");

    // 3. Agent 1 heartbeats
    let renewed = store1
        .renew_lease("task-42", "agent-1", 120)
        .expect("Heartbeat renew");
    assert!(renewed.is_active());

    // 4. Agent 2 cannot release Agent 1's lease
    assert!(store2.release_lease("task-42", "agent-2", false).is_err());

    // 5. Agent 1 gracefully releases lease
    store1
        .release_lease("task-42", "agent-1", false)
        .expect("Release lease");
    assert!(store2.get_lease("task-42").unwrap().is_none());

    // 6. Agent 2 can now claim lease
    let l2 = store2
        .acquire_lease("task-42", "agent-2", &base, 60)
        .expect("Agent 2 acquire after release");
    assert_eq!(l2.agent_id, "agent-2");
}
