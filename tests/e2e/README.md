# End-to-End Tests (`tests/e2e/`)

Orchestrates multi-step integration and end-to-end tests across isolated sandboxes.

## Files

- [`main.rs`](main.rs): E2E test harness entry point declaring all test modules.
- [`agent_workflow.rs`](agent_workflow.rs): Tests the complete single-agent lifecycle: repository ingestion, workspace creation, file modifications, zero-arg diffing, commit creation, and patch export.
- [`branch_merge_workflow.rs`](branch_merge_workflow.rs): Tests multi-agent parallel branching, workspace branch tracking, fast-forward merges, true 3-way Merkle merges with 2 parents, conflict aborts, and CAS branch updates.
- [`cherry_pick_workflow.rs`](cherry_pick_workflow.rs): Tests commit cherry-picking across divergent histories, workspace delta application, `--no-commit` staging, 3-way conflict marker generation, `--continue` resolution validation, `--abort` workspace rollback, server-side direct branch cherry-picking, and strategy overrides (`--strategy ours` / `theirs`).
- [`chunking_workflow.rs`](chunking_workflow.rs): Tests FastCDC content-defined chunking on large files (>4MB), boundary shift resistance, chunk manifest generation, sub-range binary-search slicing, and end-to-end VFS hydration.
- [`fuse_mount_workflow.rs`](fuse_mount_workflow.rs): Tests interactive read-only FUSE mounting: mounting point-in-time branch snapshots, directory listing, recursive subdirectory lookup, lazy hydration, POSIX permissions (0755/0644), and clean unmounting.
- [`gc_workflow.rs`](gc_workflow.rs): Tests mark-and-sweep garbage collection across all 4 root sources (refs, Spanner leases, workspaces, keep-roots), typed Merkle DAG traversal, grace-period safety, dry-run simulation, and remote GCS pruning.
- [`gcs_workflow.rs`](gcs_workflow.rs): Tests distributed multi-agent CAS: Agent 1 pushes blobs to GCS, and Agent 2 (with an empty cold cache) lazily pulls and hydrates blobs on demand.
- [`merge_workflow.rs`](merge_workflow.rs): Tests server-side 3-way Merkle tree merging, non-conflicting parallel edits, and conflict marker insertion for competing changes.
- [`spanner_workflow.rs`](spanner_workflow.rs): Tests distributed linearizable metadata: multi-agent branch CAS contention, stale CAS rejection, and live GCP Cloud Spanner CRUD operations.
- [`workspace_mount_workflow.rs`](workspace_mount_workflow.rs): Tests interactive writeable CoW FUSE workspace mounting: live file edits with transparent copy-up, file additions, directory tree creation, file deletion with `.wh.<name>` whiteouts, file renames, $O(K)$ delta detection, commit generation, and RAII unmount guards.
