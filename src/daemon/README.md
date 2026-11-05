# `graftd`: High-Performance Monorepo & AI Agent Fleet Coordination Daemon

`graftd` is the central coordination service for distributed teams of autonomous AI coding agents working concurrently in massive repositories.

---

## 🚀 Key Capabilities

1. **Centralized Content-Addressed Storage (CAS)**:
   - High-throughput streaming blob storage (`PUT` / `GET` / `HEAD`).
   - Pure-Rust Fast Content-Defined Chunking (FastCDC) deduplication.
   - Batch missing-blob discovery (`POST /api/v1/cas/missing`).
   - In-flight cryptographic bit-rot validation.

2. **Atomic Reference Coordination**:
   - Zero-race Compare-And-Swap (CAS) branch management (`POST /api/v1/refs/cas`).
   - Prevents blind branch overwrites when multiple agents commit concurrently.
   - First-class commit metadata indexing and DAG traversal.

3. **Server-Side 3-Way AST Semantic Merges (0 Client Downloads)**:
   - In-memory Merkle DAG walking and Concrete Syntax Tree (CST) structural merge.
   - Agents send a lightweight JSON RPC containing branch names; `graftd` executes the merge on server NVMe/RAM in < 20ms and atomically advances trunk.
   - **Network transfer: 0 bytes of source files downloaded by the client.**

4. **Distributed Workspace Leases**:
   - Time-to-Live (TTL) workspace exclusivity locks.
   - Prevents conflicting agents from concurrently modifying the same physical workspace directory or task branch.
   - Automatic expiration and safe renewal / release.

5. **Pure-Rust Async Tokio Architecture**:
   - Non-blocking HTTP/1.1 REST + streaming binary data plane.
   - Keep-Alive TCP connection reuse for microsecond latency.
   - `spawn_blocking` pool isolation preventing synchronous storage I/O from starving async networking threads.

---

## 📡 API Reference

### 1. Content-Addressed Storage (CAS)
| Method | Endpoint | Description |
|---|---|---|
| `HEAD` | `/api/v1/cas/blobs/:digest` | Check if a blob exists (`200 OK` or `404 Not Found`) |
| `GET` | `/api/v1/cas/blobs/:digest` | Retrieve raw binary blob with cryptographic verification |
| `PUT` | `/api/v1/cas/blobs/:digest` | Upload raw binary blob with precomputed BLAKE3 digest |
| `POST` | `/api/v1/cas/blobs` | Upload raw binary blob (digest computed by server) |
| `POST` | `/api/v1/cas/missing` | Batch discover absent digests (`{"digests": [...]}`) |

### 2. References & Branches
| Method | Endpoint | Description |
|---|---|---|
| `GET` | `/api/v1/refs?prefix=:prefix` | List all references matching prefix |
| `GET` | `/api/v1/refs/:name` | Retrieve target commit digest for branch/tag |
| `POST` | `/api/v1/refs/cas` | Atomic Compare-And-Swap ref update (`{"ref_name", "expected", "target", "author"}`) |
| `DELETE` | `/api/v1/refs/:name?expected=:digest` | Delete reference safely if it hasn't moved |

### 3. Commits
| Method | Endpoint | Description |
|---|---|---|
| `GET` | `/api/v1/commits/:digest` | Retrieve commit metadata (parents, root tree, author, timestamp, message) |
| `POST` | `/api/v1/commits` | Record commit metadata in the index |

### 4. Distributed Workspace Leases
| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/api/v1/leases/acquire` | Acquire lease on a workspace ID with TTL (`{"workspace_id", "agent_id", "base_commit", "ttl_seconds"}`) |
| `POST` | `/api/v1/leases/renew` | Renew active lease TTL (`{"workspace_id", "agent_id", "ttl_seconds"}`) |
| `POST` | `/api/v1/leases/release` | Explicitly release lease (`{"workspace_id", "agent_id", "force"}`) |
| `GET` | `/api/v1/leases/:id` | Check active lease status and expiration |
| `GET` | `/api/v1/leases` | List all active leases across the fleet |

### 5. Server-Side AST Semantic Merge
| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/api/v1/merge` | Execute server-side 3-way AST merge and optionally advance trunk via CAS |

**Request Body**:
```json
{
  "base": "refs/heads/main",
  "ours": "refs/heads/main",
  "theirs": "refs/heads/feature-agent-42",
  "strategy": "markers",
  "author": "Graft Merge Bot <bot@graft.dev>",
  "message": "Merge feature-agent-42 into main",
  "advance_branch": "refs/heads/main"
}
```

**Response**:
```json
{
  "status": "success",
  "clean": true,
  "merged_commit": "b3:4c728e...",
  "merged_tree": "b3:89f10a...",
  "conflict_files": []
}
```

---

## 💻 CLI Usage

### Running as a Standalone Daemon (`graftd`)
```bash
# Start graftd with local NVMe CAS + SQLite metadata
graftd --port 9090 --host 0.0.0.0 --cas-dir /mnt/nvme/graft/cas --metadata-db /mnt/nvme/graft/meta.db

# Start graftd backed by tiered S3 / MinIO storage
graftd --port 9090 --remote s3://my-corp-bucket/graft-cas --metadata-db /var/graft/meta.db

# Enable bearer token authentication
export GRAFT_AUTH_TOKEN="sec-token-xyz"
graftd --port 9090 --auth-token "$GRAFT_AUTH_TOKEN"
```

### Running via Main CLI Subcommand (`graft daemon`)
```bash
graft daemon --port 9090 --host 127.0.0.1
```

### Connecting Clients to `graftd`
Any client CLI command can point to `graftd` using `--remote`:
```bash
# Point CLI commands to remote graftd instance
graft status --remote http://graftd.corp.internal:9090
graft commit -m "Agent feature commit" --remote http://graftd.corp.internal:9090
graft push --remote http://graftd.corp.internal:9090
```
