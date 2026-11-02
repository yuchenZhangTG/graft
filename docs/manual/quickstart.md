# Graft Quickstart Guide

This guide walks you through setting up Graft, choosing your storage backend (local disk vs. distributed GCS + Spanner), ingesting a codebase, mounting ephemeral Copy-on-Write workspaces, making sub-millisecond commits, and cleaning up.

---

## 1. Prerequisites & Build

Graft is written in standard Rust (edition 2021) and requires no external runtime dependencies on Linux:

```bash
# Build release binary with FUSE support
cargo build --release --features fuse

# Or verify the pre-compiled binary
graft --version
```

---

## 2. Choosing Storage: Local vs. Cloud (GCS + Spanner)

Graft uses a unified, pluggable architecture. The exact same commands work across local workstations and distributed cloud fleets.

### Option 2A: Local Storage (Default, Zero-Setup)
By default, Graft stores CAS blobs in `/tmp/graft/cas` and references in `/tmp/graft/cas/metadata`. No configuration is required.

```bash
# Optional: customize local storage directory
graft config set cas_dir /tmp/graft/cas
```

### Option 2B: Distributed Cloud Storage (GCS + Cloud Spanner)
For multi-VM agent pools, distributed CI runners, and Cloudtop environments:
- **CAS Blobs & Merkle Trees**: Tiered cache with remote Google Cloud Storage (`gs://...`).
- **Metadata & Concurrency Leases**: Google Cloud Spanner with linearizable ACID transactions.

Configure once using the persistent config file (`$HOME/.graft/config.json`) so you don't need to re-export environment variables in every shell:

```bash
# 1. Save settings to $HOME/.graft/config.json:
graft config set remote gs://my-team-bucket/graft-repo
graft config set spanner_database projects/my-project/instances/my-instance/databases/graft-metadata
graft config set cas_dir /mnt/nvme/cas  # Local NVMe L1 cache

# Verify active settings:
graft config list

# 2. One-time setup: automatically initialize Spanner database schema
graft spanner init
```

> [!TIP]
> **Zero Manual SQL & Persistent Config**: Settings saved via `graft config set` persist across terminal sessions in `$HOME/.graft/config.json`. Precedence: CLI flags (`--remote`, `--local`) > Environment variables (`GRAFT_REMOTE`) > Config file (`$HOME/.graft/config.json`) > Defaults.
> Once configured, every subsequent command (`ingest`, `mount`, `commit`, `log`, `diff`, `merge`) automatically runs in distributed cloud mode.

---

## 3. Ingesting an Existing Repository

Ingest any existing repository or directory tree into Graft CAS. Graft hashes files with chunk-level deduplication (FastCDC) and creates a cryptographic Merkle root.

The command is **the exact same** whether local or cloud storage is configured:

```bash
graft ingest /path/to/my-repo
```

- **When Local Storage (2A) is active**: Ingests into local CAS at `/tmp/graft/cas`.
- **When Cloud Storage (2B) is active**: Ingests and automatically uploads blobs to GCS with write-through local caching.

**Output**:
```text
Ingesting '/path/to/my-repo' into CAS...
✓ Ingestion complete!
  Root Digest: 384307e412eee4b31a89c8a149c4f1c9d2f3479a96e95bfbfa28387ea31008ea:420
  Total files: 25,000
  Total bytes: 142.5 MB
```

---

## 4. Spawning an Ephemeral Agent Workspace

Mount an isolated, read/write Copy-on-Write workspace in one step:

```bash
graft workspace mount task-101 --branch main --target /tmp/task-101-mount -d
```

**Output**:
```text
✓ Acquired workspace lease: agent 'agent@workstation', expires at 2026-11-02T02:00:00Z (300s TTL)
Graft writeable workspace mounted at: /tmp/task-101-mount
  Workspace: task-101
  Base tree: 384307e412eee4b3...
  Upper CoW: /tmp/graft/storage/task-101/upper
```

> [!NOTE]
> - **Runs Unprivileged (FUSE)**: Requires **no `root` or `sudo` privileges** — ideal for developers and AI agents on workstations or Cloudtop.
> - **Instant Startup ($< 50\text{ ms}$)**: In Cloud mode (Option 2B), zero files are downloaded upfront; files are lazily streamed from GCS on demand.
> - **Background Daemon (`-d`)**: Runs the mount listener in the background, freeing your terminal immediately.

---

## 5. Making Changes & Checking Status

Agents and compilers edit files directly in `/tmp/task-101-mount`. All modifications, new files, and build artifacts land in the isolated Copy-on-Write upper layer:

```bash
# Simulate agent edits
echo "pub fn multiply(a: i32, b: i32) -> i32 { a * b }" >> /tmp/task-101-mount/math.rs
```

### Inspect Modified Files in $O(\text{modified})$ Time
Because only modified files exist in `upperdir`, status detection scans only touched files (taking $< 1\text{ ms}$ even in a monorepo with 500,000 files):
```bash
graft workspace status task-101
```

### Inspect Unified Diff
```bash
graft diff --workspace task-101
```

---

## 6. Instant Commit & Branch Advancement

Commits inspect only the touched files in the upper layer, compute new Merkle branch nodes, and atomically advance the branch:

```bash
graft commit -m "Implement multiplication function" --workspace task-101
```

**Output**:
```text
Creating commit for workspace 'task-101'...
Applying 1 file change to Merkle tree...
✓ Commit successful!
  Commit:  b61f9c4de09d3957...:158
  Tree:    e583e51297522de3...:182
  Branch:  'refs/heads/main' advanced -> b61f9c4de09d3957...:158
```

- **In Local Mode (2A)**: Updates local reference files atomically via file-lock CAS.
- **In Cloud Mode (2B)**: Pushes new blobs to GCS and executes a linearizable Compare-And-Swap transaction in Cloud Spanner. If another agent committed concurrently, Graft detects it and allows automatic 3-way merging.

---

## 7. Inspecting Commit History

```bash
graft log main --limit 5
```

**Output**:
```text
Commit log for branch 'main' (HEAD -> b61f9c4de09d3957...:158):

commit b61f9c4de09d395774e8ce52bb61eed057d435fc4392e2d53c44ced68cd8e377
Author: AI Agent <agent@graft.dev>
Date:   1793342596 (Unix timestamp)
Tree:   e583e51297522de34b6cfb35b299d0edfdf60f00be7df84f5e8fe7e742b27f4e:182

    Implement multiplication function
```

---

## 8. Exporting Git-Compatible Patches

Export changes for code review systems (Critique, Gerrit, GitHub PRs):

```bash
graft export patch --workspace task-101 --output /tmp/task-101.patch
```

---

## 9. Cleaning Up Workspaces

When a task is complete, a single command unmounts the filesystem, deletes the mount directory, clears scratch storage, and releases the metadata lease:

```bash
# Destroy by workspace ID or mount directory path:
graft workspace destroy task-101
# Or:
graft workspace destroy /tmp/task-101-mount
```

**Output**:
```text
✓ Workspace 'task-101' cleaned up successfully.
✓ Released workspace lease for 'task-101'
```

---

## 10. Inspecting Code & Agent Workspaces in the Web UI

Graft includes an embedded GitHub-style web interface for visual code search, repository browsing, diff inspection, and monitoring active AI agent workspaces:

```bash
graft web
```

**Output**:
```text
╔════════════════════════════════════════════════════════════════════╗
║                   🌱 GRAFT WEB DASHBOARD                           ║
╠════════════════════════════════════════════════════════════════════╣
║  Local URL:    http://localhost:8080                              ║
║  Bind Addr:    http://0.0.0.0:8080                                 ║
║  Default Ref:  main                                                ║
║  Storage Dir:  /tmp/graft/storage                                  ║
╚════════════════════════════════════════════════════════════════════╝
Ready for Code Search, Tree Browsing & Workspace Monitoring. Press Ctrl-C to stop.
```

Open `http://localhost:8080` (or `http://<cloudtop-hostname>:8080`) in your browser to:
- **Browse Files & Commits**: Explore directories, syntax-highlighted files with line numbers, and commit logs.
- **Search Code**: Press `/` to trigger instant, sub-second code search across the Merkle tree with line numbers and snippets.
- **Monitor Active Agent Workspaces**: Inspect active workspaces in `/workspaces` and review real-time, uncommitted Copy-on-Write diffs as agents edit code!
