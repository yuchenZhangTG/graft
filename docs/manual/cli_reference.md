# Graft CLI Reference Manual

Comprehensive command-line interface documentation for the `graft` binary.

---

## Global Storage & Configuration Flags

All commands accept these storage options (or read them from environment variables):

| Flag | Env Var | Default | Description |
| :--- | :--- | :--- | :--- |
| `--cas-dir <DIR>` | `GRAFT_CAS_DIR` | `/tmp/graft/cas` | Root on-disk path for local NVMe Content-Addressed Storage (L1 cache). |
| `--remote <URL>` | `GRAFT_REMOTE` | *None* | Remote cloud store URL (e.g. `gs://<bucket>/<prefix>`) for L2 persistence. |
| `--spanner-database <PATH>` | `GRAFT_SPANNER_DATABASE` | *None* | Cloud Spanner database resource path for linearizable branch and lease metadata. |

---

## Commands

### `graft ingest`
Ingests a physical repository directory into CAS, computing BLAKE3 digests and deterministic Merkle directory manifests.

```bash
graft ingest [OPTIONS] [PATH]
```
- `[PATH]`: Path to directory to ingest (defaults to current directory `.`).
- Flags: `--cas-dir`, `--remote`.

---

### `graft workspace`
Manages the lifecycle of ephemeral Copy-on-Write (CoW) workspaces for AI agents.

#### `graft workspace create`
Spawns an isolated workspace directory structure (`upperdir`, `workdir`, `merged`) in $< 50\text{ ms}$.

```bash
graft workspace create [OPTIONS] <ID>
```
- `<ID>`: Unique workspace identifier (e.g. `task-101`, `agent-auth`).
- `-b, --branch <BRANCH>`: Branch to track and branch off of (e.g. `main`, `feature-login`).
- `-c, --commit <COMMIT>`: Explicit commit or tree digest to base the workspace on.
- `-l, --lower <DIR>`: Path to base read-only directory.
- `-t, --target <DIR>`: Path to target merged mount directory (defaults to `<storage_dir>/<ID>/merged`).
- `--agent-id <ID>`: Agent identity string for lease locking (defaults to `$USER@$HOSTNAME`).
- `--lease-ttl <SECS>`: Workspace lease TTL in seconds (default: 300).
- `--no-lease`: Skip acquiring a lease on this workspace.
- `--storage-dir <DIR>`: Base directory for workspace CoW layers (default: `/tmp/graft/storage`).

#### `graft workspace status`
Inspects created, modified, or deleted files inside `upperdir` in $O(\text{modified})$ time without scanning the base repository. Supports both Linux kernel whiteout character devices (`0, 0`) and user-space `.wh.<name>` tombstone markers.

```bash
graft workspace status [OPTIONS] [ID]
```

#### `graft workspace mount`
Mounts the agent workspace as an interactive writeable Copy-on-Write (CoW) filesystem using unprivileged user-space FUSE or kernel OverlayFS. Automatically provides transparent copy-up on file edits, whiteout tombstone creation on file unlinks, opaque directory markers, and stable Inode numbers across copy-up.

```bash
graft workspace mount [OPTIONS] [ID]
```
- `[ID]`: Workspace identifier (defaults to `default`).
- `-d, --daemon`: Run the mount session in a background daemon process.
- `-t, --target <DIR>`: Custom mount directory (defaults to `<storage_dir>/<ID>/merged`).
- `--backend <BACKEND>`: Mount backend: `auto` (default, selects FUSE for unprivileged users, OverlayFS if root), `fuse`, or `overlay`.
- `--storage-dir <DIR>`: Base storage directory for CoW layers.

#### `graft workspace unmount`
Safely unmounts an active workspace mount and terminates any background mount daemon.

```bash
graft workspace unmount [OPTIONS] [ID]
```
- `[ID]`: Workspace identifier (defaults to `default`).
- `-l, --lazy`: Perform lazy unmount (`-z` / `-l`).
- `--storage-dir <DIR>`: Base storage directory for CoW layers.

#### `graft workspace list`
Lists all workspaces with their current branch, agent owner, base commit, and active mount status.

```bash
graft workspace list [OPTIONS]
```

#### `graft workspace destroy`
Safely tears down and cleans up an agent workspace's temporary CoW files, auto-unmounting the target filesystem if mounted, and releases any associated leases.

```bash
graft workspace destroy [OPTIONS] [ID]
```

---

### `graft commit`
Creates an immutable Commit object snapshot from an active workspace in $O(\text{modified})$ time.

```bash
graft commit [OPTIONS] -m <MESSAGE>
```
- `-m, --message <MSG>`: Commit description message.
- `-w, --workspace <ID>`: Unique workspace identifier (defaults to `default`).
- `-b, --branch <BRANCH>`: Branch name to advance (defaults to workspace tracked branch).
- `-a, --author <STR>`: Author string (defaults to `AI Agent <agent@graft.dev>`).
- `-p, --parent <DIGEST>`: Explicit parent commit digest.
- `--allow-empty`: Record commit even if no files were touched in the workspace.
- `--storage-dir <DIR>`: Base directory for workspace storage layers.

---

### `graft branch`
Inspects, creates, advances, or deletes branch references.

```bash
graft branch [OPTIONS] [NAME] [START_POINT]
```
- `[NAME]`: Branch name to create, update, or delete (e.g. `feature-auth`).
- `[START_POINT]`: Commit digest or existing branch to branch off of (defaults to `main`).
- `-l, --list`: List all branches and their corresponding commit digests.
- `-d, --delete`: Delete the specified branch reference.
- `-f, --force`: Force overwrite existing branch head pointer.

---

### `graft merge`
Executes server-side 3-way Merkle tree merging or fast-forward between branches or commits in $O(\text{changed})$ time.

```bash
graft merge [OPTIONS] [SOURCE]
```
- `[SOURCE]`: Source branch or commit digest to merge into the target branch.
- `-i, --into <TARGET>`: Target branch to merge into (defaults to active workspace branch or `main`).
- `-m, --message <MSG>`: Commit message for the merge commit.
- `--no-ff`: Force creation of a two-parent merge commit even if fast-forward is possible.
- `--allow-conflicts`: Commit files with `<<<<<<< ours` conflict markers rather than aborting.

---

### `graft cherry-pick`
Applies an individual commit snapshot from another branch onto an active workspace or directly onto a target branch in CAS via 3-way Merkle merge.

```bash
graft cherry-pick [OPTIONS] [COMMIT]
```
- `[COMMIT]`: Commit digest or branch name to cherry-pick (required unless `--abort` or `--continue`).
- `-w, --workspace <ID>`: Target workspace identifier (defaults to `default`).
- `-b, --branch <BRANCH>`: Target branch in metadata store for direct server-side CAS cherry-picking.
- `--no-commit`: Apply changes to workspace upper layer without recording a commit.
- `-m, --message <MSG>`: Custom commit message (defaults to original commit message).
- `-a, --author <STR>`: Author string (defaults to original author or `Cherry-Pick <cherry-pick@graft.dev>`).
- `--mainline <N>`: Parent index (1-based, default `1`) for cherry-picking merge commits.
- `--strategy <STRAT>`: Conflict resolution strategy: `markers` (default), `ours`, `theirs`, or `fail`.
- `--continue`: Scan workspace, verify all `<<<<<<< ours` conflict markers are resolved, and finalize the cherry-pick commit.
- `--abort`: Discard all in-progress cherry-pick changes, restore workspace upper directory, and delete conflict state.

---

### `graft mount`
Mounts a point-in-time branch, commit, or Merkle tree as an interactive read-only FUSE virtual filesystem.

```bash
graft mount [OPTIONS] <MOUNTPOINT>
```
- `<MOUNTPOINT>`: Target directory for the FUSE mount (e.g. `/tmp/mnt`).
- `-c, --commit <SPEC>`: Branch name (`main`), commit digest, or tree manifest digest to mount (default: `main`).
- `-d, --daemon`: Run FUSE mount daemon in the background.

---

### `graft unmount`
Gracefully unmounts an active Graft FUSE mount point.

```bash
graft unmount [OPTIONS] <MOUNTPOINT>
```
- `<MOUNTPOINT>`: Mounted directory to unmount.
- `-l, --lazy`: Perform lazy unmount (`-z`).

---

### `graft diff`
Generates unified diffs of an active workspace or between two arbitrary tree digests.

```bash
graft diff [OPTIONS]
```
- `-w, --workspace <ID>`: Workspace identifier to inspect.
- `--base <DIGEST>`: Base tree digest for comparing two snapshots.
- `--target <DIGEST>`: Target tree digest for comparing two snapshots.

---

### `graft log`
Displays commit ancestry graphs from newest to oldest.

```bash
graft log [OPTIONS] [COMMIT]
```
- `[COMMIT]`: Branch name or commit digest to start walking from (default: `main`).
- `-n, --limit <COUNT>`: Maximum number of commits to display.

---

### `graft export`
Exports repository snapshots or patches.

#### `graft export patch`
Outputs a standard Git-compatible unified `.patch` file of workspace changes.

```bash
graft export patch --workspace <ID> --output <FILE>
```

#### `graft export tree`
Materializes a full directory tree from a Merkle digest into a local directory with restored POSIX permissions.

```bash
graft export tree --tree <DIGEST> --target <DIR>
```

---

### `graft lease`
Coordinates distributed agent workspace leases to prevent multiple agents from editing the same task simultaneously.

```bash
graft lease acquire <WORKSPACE> [--ttl-secs 300]
graft lease heartbeat <WORKSPACE> [--ttl-secs 300]
graft lease release <WORKSPACE>
graft lease status <WORKSPACE>
graft lease list
```

---

### `graft gc`
Performs mark-and-sweep garbage collection to reclaim storage from unreferenced blobs in CAS.

```bash
graft gc [OPTIONS]
```
- `--dry-run`: Previews reclaimable storage and object counts without deleting any blobs.
- `--grace-period-hours <HOURS>`: Grace period to protect recently created unreferenced blobs (default: `24`).
- `--grace-period-secs <SECS>`: Explicit grace period in seconds (overrides `--grace-period-hours`).
- `--prune-remote`: Also delete unreferenced blobs from remote storage (e.g. GCS).
- `--allow-empty-roots`: Safety override allowing GC to proceed even if zero live roots were discovered.
- `--keep-root <SPEC>`: Explicit commit or tree digest to treat as an additional live root (repeatable).

---

### `graft spanner`
Manages Cloud Spanner database schema initialization and DDL generation for distributed fleet deployments.

```bash
# Automatically provision all 4 required tables in Cloud Spanner
graft spanner init [--database <PROJECT/INSTANCE/DATABASE>] [--dry-run]

# Print canonical DDL statements (e.g. to pipe to gcloud or inspect)
graft spanner ddl [--prefix <TABLE_PREFIX>]
```
- `init`: Connects to Google Cloud Spanner via REST API and creates `graft_refs`, `graft_commits`, `graft_commit_parents`, and `graft_workspace_leases`.
- `ddl`: Outputs raw SQL DDL statements for manual verification or automation pipelines.

---

### `graft config`
Manages persistent user configuration stored in `$HOME/.graft/config.json`. Eliminates the need to export environment variables on every session.

```bash
# Set configuration keys
graft config set remote gs://my-team-bucket/graft-repo
graft config set spanner_database projects/my-project/instances/my-instance/databases/graft-metadata
graft config set cas_dir /mnt/nvme/cas

# Get a configuration value
graft config get remote

# Unset a configuration key
graft config unset remote

# List all current configuration settings and active resolved values
graft config list

# Print path to the configuration file
graft config path
```

#### Precedence Order:
1. Command-line flags (e.g. `--remote gs://...`)
2. Environment variables (e.g. `GRAFT_REMOTE`, `GRAFT_SPANNER_DATABASE`)
3. User configuration file (`$HOME/.graft/config.json`)
4. Built-in defaults (`/tmp/graft/cas`, `/tmp/graft/storage`)

---

### `graft web`
Launches the embedded GitHub-style web interface for instant code search, repository browsing, commit history DAGs, and real-time AI agent workspace monitoring.

```bash
# Start Web UI on default port 8080 (binds to 0.0.0.0)
graft web

# Start on custom port and branch
graft web --port 3000 --branch feature-1

# Automatically open browser in desktop environment
graft web --open
```

#### Web Interface Views:
- **Repository Browser (`/tree/<branch>/<path>`)**: GitHub-style directory table with folder and file Octicons, file sizes, BLAKE3 digests, and embedded README.md rendering.
- **File Viewer (`/blob/<branch>/<path>`)**: Line-numbered code viewer with raw copy buttons, FastCDC chunk status, and anchor links (`#L42`).
- **Instant Code Search (`/search?q=<query>`)**: Sub-second full-text and path search across the entire Merkle tree with line numbers and highlighted matches.
- **Commit History & Diffs (`/commits` and `/diff/<commit>`)**: Ancestry log and colored unified diff viewers.
- **Active Workspaces & Live Diffs (`/workspaces` and `/workspaces/<id>/diff`)**: Real-time dashboard showing active agent workspaces, mount PIDs, and live uncommitted Copy-on-Write modifications.
- **REST API (`/api/...`)**: Programmatic JSON endpoints (`/api/info`, `/api/tree`, `/api/blob`, `/api/search`, `/api/commits`, `/api/diff`, `/api/workspaces`) for AI agents and IDE integrations.
