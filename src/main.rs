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

//! # Graft CLI: Command-Line Interface
//!
//! Provides developer-facing and agent-facing commands for:
//! - Ingesting repositories into Content-Addressed Storage (`graft ingest`)
//! - Creating and managing ephemeral Copy-on-Write workspaces (`graft workspace
//!   create/status/destroy`)
//! - Committing changes in O(K) time without full repo scans (`graft commit`)
//! - Inspecting commit history and 3-way Merkle merges (`graft log`, `graft merge`)
//! - Two-tier storage with local NVMe L1 cache and remote GCP Cloud Storage (`gs://...`)
//! - Managing branch references and commit graphs (`graft branch`)

use clap::{Parser, Subcommand};
use graft::cas::{
    apply_changes_to_tree, apply_tree_delta_to_workspace, cherry_pick_commit, extract_tree_delta,
    find_common_ancestor, get_commit_history, merge_trees, BlobStore, Commit,
    ConflictStrategy, Digest, GcsBlobStore, LocalBlobStore, TreeDelta,
};
use graft::git::{
    compute_diff_stats, diff_trees, diff_workspace, export_patch_file, export_tree_to_dir,
};
use graft::metadata::{
    normalize_ref_name, resolve_ref, validate_branch_name, MetadataStore,
};
use graft::workspace::{CherryPickState, WorkspaceManager};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use graft::config::StorageArgs;

fn resolve_storage_dir(raw_dir: &Path) -> PathBuf {
    if raw_dir != Path::new("/tmp/graft/storage") {
        return raw_dir.to_path_buf();
    }
    if let Ok(env_dir) = std::env::var("GRAFT_STORAGE_DIR") {
        return PathBuf::from(env_dir);
    }
    let cfg = graft::config::GraftConfigFile::load();
    if let Some(cfg_dir) = cfg.storage_dir {
        return cfg_dir;
    }
    PathBuf::from("/tmp/graft/storage")
}

#[derive(Parser)]
#[command(name = "graft")]
#[command(about = "Graft: High-Performance Virtualized Source Control for AI Agents & Monorepos")]
#[command(version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Workspace lifecycle operations for AI agents
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCommands,
    },
    /// Low-level Content-Addressed Storage operations
    Cas {
        #[command(subcommand)]
        action: CasCommands,
    },
    /// Ingest a repository directory into Graft CAS and generate a Merkle manifest
    Ingest {
        #[arg(
            default_value = ".",
            help = "Path to directory to ingest (defaults to current dir '.')"
        )]
        path: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Create a new immutable Commit snapshot from an active agent workspace (O(K) time)
    Commit {
        #[arg(short, long, help = "Commit description message")]
        message: String,

        #[arg(
            short,
            long,
            default_value = "default",
            help = "Unique identifier of the agent workspace (defaults to 'default')"
        )]
        workspace: String,

        #[arg(
            short,
            long,
            help = "Branch name to advance with this commit (defaults to workspace tracked branch)"
        )]
        branch: Option<String>,

        #[arg(short, long, default_value = "AI Agent <agent@graft.dev>", help = "Author string")]
        author: String,

        #[arg(
            long,
            help = "Base root tree digest (optional, auto-loaded from workspace configuration)"
        )]
        base_tree: Option<String>,

        #[arg(short, long, help = "Parent commit digest (optional)")]
        parent: Option<String>,

        #[arg(long, help = "Allow recording a commit even if no changes were made in workspace")]
        allow_empty: bool,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Inspect unified diff of an active workspace or between two tree digests
    Diff {
        #[arg(
            short,
            long,
            default_value = "default",
            help = "Workspace identifier to inspect (defaults to 'default')"
        )]
        workspace: Option<String>,

        #[arg(
            long,
            help = "Path to read-only lower directory (auto-loaded from workspace config)"
        )]
        lower: Option<PathBuf>,

        #[arg(long, help = "Base Merkle tree digest (for comparing two snapshots)")]
        base: Option<String>,

        #[arg(long, help = "Target Merkle tree digest (for comparing two snapshots)")]
        target: Option<String>,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Display commit ancestry history
    Log {
        #[arg(
            default_value = "main",
            help = "Branch name or commit digest to start walking from (defaults to 'main')"
        )]
        commit: String,

        #[arg(short, long, help = "Maximum number of commits to display")]
        limit: Option<usize>,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Export patches or materialize repository trees
    Export {
        #[command(subcommand)]
        action: ExportCommands,
    },
    /// Perform a 3-way Merkle tree merge or fast-forward between branches or commits
    Merge {
        #[arg(help = "Source branch or commit digest to merge (e.g. 'feature-1' or commit hash)")]
        source: Option<String>,

        #[arg(
            long,
            short = 'i',
            help = "Target branch to merge into (defaults to current workspace branch or 'main')"
        )]
        into: Option<String>,

        #[arg(long, help = "Legacy: explicit our commit digest")]
        ours: Option<String>,

        #[arg(long, help = "Legacy: explicit their commit digest")]
        theirs: Option<String>,

        #[arg(long, help = "Explicit common ancestor base commit (auto-detected if omitted)")]
        base: Option<String>,

        #[arg(short, long, help = "Commit message for the merge commit")]
        message: Option<String>,

        #[arg(
            short,
            long,
            default_value = "Merge Coordinator <merge@graft.dev>",
            help = "Author string"
        )]
        author: String,

        #[arg(long, help = "Disable fast-forward merge and always create a merge commit")]
        no_ff: bool,

        #[arg(
            long,
            help = "Allow committing files with conflict markers (by default, aborts on conflict)"
        )]
        allow_conflicts: bool,

        #[arg(
            long,
            default_value = "default",
            help = "Active workspace identifier (to detect tracked branch)"
        )]
        workspace: String,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Cherry-pick a commit snapshot onto an active workspace or branch
    CherryPick {
        #[arg(
            help = "Commit digest or branch reference to cherry-pick (required unless --abort or --continue)"
        )]
        commit: Option<String>,

        #[arg(
            short,
            long,
            help = "Target workspace identifier (defaults to 'default' when neither --workspace nor --branch is specified)"
        )]
        workspace: Option<String>,

        #[arg(short, long, help = "Target branch in metadata store for direct CAS cherry-picking")]
        branch: Option<String>,

        #[arg(long, help = "Apply changes to workspace upper layer without recording a commit")]
        no_commit: bool,

        #[arg(short, long, help = "Custom commit message (defaults to original commit message)")]
        message: Option<String>,

        #[arg(
            short,
            long,
            default_value = "Cherry-Pick <cherry-pick@graft.dev>",
            help = "Author string"
        )]
        author: String,

        #[arg(
            long,
            default_value = "1",
            help = "Parent index (1-based) for cherry-picking merge commits"
        )]
        mainline: Option<usize>,

        #[arg(
            long,
            default_value = "markers",
            help = "Conflict resolution strategy: 'markers', 'ours', 'theirs', or 'fail'"
        )]
        strategy: String,

        #[arg(
            long,
            help = "Abort an in-progress cherry-pick with unresolved conflicts and restore workspace"
        )]
        abort: bool,

        #[arg(long, help = "Continue and finalize a cherry-pick after resolving conflict markers")]
        r#continue: bool,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// List, create, or delete branch references
    Branch {
        #[arg(help = "Branch name to create or delete (e.g. 'feature-x')")]
        name: Option<String>,

        #[arg(help = "Starting point / target commit digest or branch name (defaults to 'main')")]
        start_point: Option<String>,

        #[arg(long, help = "Explicit starting point branch or commit (alias for start_point)")]
        from: Option<String>,

        #[arg(short, long, help = "List all branches")]
        list: bool,

        #[arg(short, long, help = "Delete branch reference")]
        delete: bool,

        #[arg(short, long, help = "Force update existing branch reference")]
        force: bool,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Manage distributed agent workspace leases
    Lease {
        #[command(subcommand)]
        action: LeaseCommands,
    },
    /// Mount a snapshot or branch as a virtual read-only FUSE filesystem
    Mount {
        #[arg(help = "Path to mount directory (e.g. /tmp/graft-mount)")]
        mountpoint: PathBuf,

        #[arg(
            short,
            long,
            default_value = "main",
            help = "Branch name, commit digest, or tree digest to mount (defaults to 'main')"
        )]
        commit: String,

        #[arg(short = 'd', long, help = "Run FUSE mount daemon in background")]
        daemon: bool,

        #[arg(
            long,
            help = "Internal flag: run mount session in background child process",
            hide = true
        )]
        internal_daemon: bool,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Unmount an active Graft FUSE mount
    Unmount {
        #[arg(help = "Path to mounted directory to unmount")]
        mountpoint: PathBuf,

        #[arg(short, long, help = "Perform lazy unmount (-z)")]
        lazy: bool,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Reclaim disk and cloud storage by purging unreferenced blobs from CAS
    Gc {
        #[arg(
            long,
            help = "Perform analysis and report reclaimable storage without deleting blobs"
        )]
        dry_run: bool,

        #[arg(
            long,
            default_value = "24",
            help = "Grace period in hours to protect recently created unreferenced blobs"
        )]
        grace_period_hours: u64,

        #[arg(
            long,
            help = "Explicit grace period in seconds (overrides --grace-period-hours if specified)"
        )]
        grace_period_secs: Option<u64>,

        #[arg(long, help = "Also delete unreferenced blobs from remote storage (e.g. GCS)")]
        prune_remote: bool,

        #[arg(
            long,
            help = "Safety override: allow garbage collection even if zero live roots were found"
        )]
        allow_empty_roots: bool,

        #[arg(
            long = "keep-root",
            help = "Explicit commit or tree digest to treat as an additional live root (repeatable)"
        )]
        keep_roots: Vec<String>,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers (to discover active workspaces)"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Cloud Spanner database administration
    Spanner {
        #[command(subcommand)]
        action: SpannerCommands,
    },
    /// Manage persistent Graft configuration ($HOME/.graft/config.json)
    Config {
        #[command(subcommand)]
        action: ConfigCommands,
    },
    /// Launch the GitHub-style Graft Web UI for code search, repository browsing, diffs, and workspace monitoring
    Web {
        #[arg(
            short,
            long,
            default_value = "8080",
            help = "Port number to listen on (e.g. 8080)"
        )]
        port: u16,

        #[arg(
            long,
            default_value = "0.0.0.0",
            help = "Host address to bind to (e.g. 0.0.0.0 or 127.0.0.1)"
        )]
        host: String,

        #[arg(
            short,
            long,
            default_value = "main",
            help = "Default branch or commit to view (defaults to 'main')"
        )]
        branch: String,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[arg(long, help = "Automatically open browser in local session")]
        open: bool,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Start the graftd server-side coordination daemon
    Daemon {
        #[command(flatten)]
        daemon: graft::daemon::DaemonArgs,

        #[command(flatten)]
        storage: StorageArgs,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Set a configuration value (e.g. 'graft config set remote gs://bucket/repo')
    Set {
        #[arg(help = "Configuration key ('remote', 'spanner_database', 'cas_dir', 'storage_dir', 'agent_id')")]
        key: String,
        #[arg(help = "Configuration value to set")]
        value: String,
    },
    /// Get a configuration value
    Get {
        #[arg(help = "Configuration key to inspect")]
        key: String,
    },
    /// Remove a configuration key
    Unset {
        #[arg(help = "Configuration key to remove")]
        key: String,
    },
    /// List all configured settings
    List,
    /// Print path to the configuration file
    Path,
}

#[derive(Subcommand)]
pub enum SpannerCommands {
    /// Initialize Graft tables in Cloud Spanner database
    Init {
        #[arg(
            long,
            env = "GRAFT_SPANNER_DATABASE",
            help = "Cloud Spanner database path (e.g. 'projects/p/instances/i/databases/d')"
        )]
        database: Option<String>,

        #[arg(long, help = "Print DDL statements without executing them")]
        dry_run: bool,
    },
    /// Print canonical Cloud Spanner DDL schema statements
    Ddl {
        #[arg(long, help = "Optional table prefix (e.g. 'dev_')")]
        prefix: Option<String>,
    },
}

#[derive(Subcommand)]
enum LeaseCommands {
    /// Acquire an exclusive lease on a workspace
    Acquire {
        #[arg(help = "Workspace identifier to lease (e.g. 'default', 'task-100')")]
        workspace: String,

        #[arg(
            short,
            long,
            env = "GRAFT_AGENT_ID",
            help = "Agent identifier holding lease (defaults to USER@HOSTNAME)"
        )]
        agent_id: Option<String>,

        #[arg(short, long, help = "Base commit digest this workspace is based on")]
        commit: Option<String>,

        #[arg(
            long,
            default_value = "300",
            env = "GRAFT_LEASE_TTL_SECS",
            help = "Lease duration in seconds"
        )]
        ttl_secs: u64,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Renew / heartbeat an active workspace lease
    Heartbeat {
        #[arg(help = "Workspace identifier to heartbeat")]
        workspace: String,

        #[arg(short, long, env = "GRAFT_AGENT_ID", help = "Agent identifier holding lease")]
        agent_id: Option<String>,

        #[arg(
            long,
            default_value = "300",
            env = "GRAFT_LEASE_TTL_SECS",
            help = "Lease duration extension in seconds"
        )]
        ttl_secs: u64,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Release a workspace lease
    Release {
        #[arg(help = "Workspace identifier to release")]
        workspace: String,

        #[arg(short, long, env = "GRAFT_AGENT_ID", help = "Agent identifier holding lease")]
        agent_id: Option<String>,

        #[arg(short, long, help = "Force release even if owned by another agent")]
        force: bool,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Show current lease status for a workspace
    Status {
        #[arg(help = "Workspace identifier to check")]
        workspace: String,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// List all workspace leases across the fleet
    List {
        #[command(flatten)]
        storage: StorageArgs,
    },

}

#[derive(Subcommand)]
enum ExportCommands {
    /// Export workspace or tree diff as a standard .patch file
    Patch {
        #[arg(
            short,
            long,
            default_value = "default",
            help = "Workspace identifier to export (defaults to 'default')"
        )]
        workspace: Option<String>,

        #[arg(
            long,
            help = "Path to read-only lower directory (auto-loaded from workspace config)"
        )]
        lower: Option<PathBuf>,

        #[arg(long, help = "Base tree digest")]
        base: Option<String>,

        #[arg(long, help = "Target tree digest")]
        target: Option<String>,

        #[arg(short, long, help = "Output patch file path (defaults to stdout if omitted)")]
        output: Option<PathBuf>,

        #[arg(
            long,
            default_value = "/tmp/graft/storage",
            help = "Base directory for workspace CoW layers"
        )]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Materialize a complete Merkle tree snapshot into a destination directory
    Tree {
        #[arg(short, long, help = "Root tree digest to export")]
        tree: String,

        #[arg(long, help = "Destination directory to write files to")]
        target: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },

}

#[derive(Subcommand)]
enum WorkspaceCommands {
    /// Initialize an isolated Copy-on-Write workspace for an agent in O(1) time
    Create {
        #[arg(
            default_value = "default",
            help = "Unique identifier for this agent task (defaults to 'default')"
        )]
        id: String,

        #[arg(
            short,
            long,
            help = "Base commit, branch, or tree hash (auto-ingests lower directory if omitted)"
        )]
        commit: Option<String>,

        #[arg(
            short,
            long,
            help = "Branch name to check out and track in this workspace (e.g. 'feature-1')"
        )]
        branch: Option<String>,

        #[arg(
            short,
            long,
            default_value = ".",
            help = "Path to read-only lower directory (defaults to current dir '.')"
        )]
        lower: PathBuf,

        #[arg(
            short,
            long,
            help = "Target path where the merged workspace will be mounted (defaults to '<storage_dir>/<id>/merged')"
        )]
        target: Option<PathBuf>,

        #[arg(long, env = "GRAFT_AGENT_ID", help = "Agent identifier claiming workspace lease")]
        agent_id: Option<String>,

        #[arg(
            long,
            default_value = "300",
            env = "GRAFT_LEASE_TTL_SECS",
            help = "Lease duration in seconds"
        )]
        lease_ttl: u64,

        #[arg(long, help = "Disable automatic distributed lease acquisition")]
        no_lease: bool,

        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Scan and display the delta/changeset in an active workspace (O(K) time)
    Status {
        #[arg(
            default_value = "default",
            help = "Unique identifier for the agent task (defaults to 'default')"
        )]
        id: String,

        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Mount an agent workspace as an interactive writeable CoW filesystem
    Mount {
        #[arg(
            default_value = "default",
            help = "Unique identifier for the agent workspace (defaults to 'default')"
        )]
        id: String,

        #[arg(
            short = 't',
            long,
            help = "Custom mount directory (defaults to '<storage_dir>/<id>/merged')"
        )]
        target: Option<PathBuf>,

        #[arg(short = 'd', long, help = "Run workspace mount daemon in background")]
        daemon: bool,

        #[arg(
            long,
            help = "Internal flag: run mount session in background child process",
            hide = true
        )]
        internal_daemon: bool,

        #[arg(
            long,
            default_value = "auto",
            help = "Mount backend: 'auto', 'fuse' (user-space CoW), or 'overlay' (kernel OverlayFS)"
        )]
        backend: String,

        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Unmount an active writeable agent workspace
    Unmount {
        #[arg(
            default_value = "default",
            help = "Unique identifier for the agent workspace (defaults to 'default')"
        )]
        id: String,

        #[arg(short, long, help = "Perform lazy unmount (-z / -l)")]
        lazy: bool,

        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,
    },
    /// List all workspaces and their current mount / lease status
    List {
        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,
    },
    /// Tear down an ephemeral agent workspace
    Destroy {
        #[arg(
            default_value = "default",
            help = "Unique identifier for the agent task (defaults to 'default')"
        )]
        id: String,

        #[arg(long, env = "GRAFT_AGENT_ID", help = "Agent identifier releasing workspace lease")]
        agent_id: Option<String>,

        #[arg(short, long, help = "Force release workspace lease even if held by another agent")]
        force: bool,

        #[arg(long, default_value = "/tmp/graft/storage", help = "Base directory for CoW layers")]
        storage_dir: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },

}

#[derive(Subcommand)]
enum CasCommands {
    /// Compute the BLAKE3 digest of a local file
    Hash {
        #[arg(help = "Path to file")]
        file: PathBuf,
    },
    /// Ingest a file into CAS (local cache and optional remote store)
    Put {
        #[arg(help = "Path to file")]
        file: PathBuf,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Pull a blob from remote storage into local CAS cache
    Pull {
        #[arg(help = "Digest to pull (e.g. 64-char hex)")]
        digest: String,

        #[command(flatten)]
        storage: StorageArgs,
    },
    /// Push a local blob to remote storage
    Push {
        #[arg(help = "Digest to push (e.g. 64-char hex)")]
        digest: String,

        #[command(flatten)]
        storage: StorageArgs,
    },

}

fn get_default_agent_id(custom: Option<String>) -> String {
    if let Some(c) = custom {
        if !c.trim().is_empty() {
            return c;
        }
    }
    if let Ok(env_id) = std::env::var("GRAFT_AGENT_ID") {
        if !env_id.trim().is_empty() {
            return env_id;
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "agent".to_string());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string());
    format!("{}@{}", user, host)
}

/// Resolves a revision spec (branch name, ref path, or commit digest) against CAS and metadata
/// stores.
///
/// Returns `(canonical_ref, commit_digest, commit_object)`.
fn resolve_revision(
    spec: &str,
    blob_store: &dyn BlobStore,
    meta_store: &dyn MetadataStore,
) -> anyhow::Result<(Option<String>, Digest, Commit)> {
    // 1. Try parsing as direct Digest (<hash>:<size>)
    if let Ok(d) = Digest::parse(spec) {
        if let Some(bytes) = blob_store.get(&d)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((None, d, commit));
            }
        }
    }
    // 2. Try parsing as 64-char hex digest
    if spec.len() == 64 && spec.chars().all(|c| c.is_ascii_hexdigit()) {
        let d = Digest::from_hex(spec, 0).map_err(|e| anyhow::anyhow!("Invalid hex: {}", e))?;
        if let Some(bytes) = blob_store.get(&d)? {
            if let Ok(commit) = Commit::deserialize(&bytes) {
                return Ok((None, d, commit));
            }
        }
    }

    // 3. Try resolving as a branch / reference in metadata store
    if let Some((canonical_ref, commit_digest)) = resolve_ref(meta_store, spec)? {
        let bytes = blob_store.get(&commit_digest)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Commit {} referenced by '{}' not found in CAS",
                commit_digest,
                canonical_ref
            )
        })?;
        let commit = Commit::deserialize(&bytes).map_err(|e| {
            anyhow::anyhow!("Failed to deserialize commit {}: {}", commit_digest, e)
        })?;
        return Ok((Some(canonical_ref), commit_digest, commit));
    }

    anyhow::bail!("Revision or branch '{}' not found in repository metadata or CAS", spec);
}

/// Safely canonicalizes a path, falling back to parent resolution if the endpoint is disconnected
/// (ENOTCONN).
fn safe_canonicalize(path: &std::path::Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(path) {
        return canon;
    }
    if let Some(parent) = path.parent() {
        if let Ok(canon_parent) = fs::canonicalize(parent) {
            if let Some(name) = path.file_name() {
                return canon_parent.join(name);
            }
        }
    }
    path.to_path_buf()
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Workspace { action } => match action {
            WorkspaceCommands::Create {
                id,
                commit,
                branch,
                lower,
                target,
                agent_id,
                lease_ttl,
                no_lease,
                storage_dir,
                storage,
            } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let store = storage.open_store()?;
                let meta_store = storage.open_metadata()?;

                let canonical_lower = fs::canonicalize(&lower).unwrap_or(lower);
                let target_dir = target.unwrap_or_else(|| storage_dir.join(&id).join("merged"));

                let mut tracked_branch = None;
                let mut tracked_base_commit = None;

                let base_tree = if let Some(b_spec) = branch {
                    let (canon_ref, c_digest, commit_obj) =
                        resolve_revision(&b_spec, store.as_ref(), meta_store.as_ref())?;
                    tracked_branch = canon_ref;
                    tracked_base_commit = Some(c_digest.to_string());
                    commit_obj.root_tree_digest.to_hex()
                } else if let Some(c_spec) = commit {
                    if let Ok((canon_ref, c_digest, commit_obj)) =
                        resolve_revision(&c_spec, store.as_ref(), meta_store.as_ref())
                    {
                        tracked_branch = canon_ref;
                        tracked_base_commit = Some(c_digest.to_string());
                        commit_obj.root_tree_digest.to_hex()
                    } else if let Ok(d) = Digest::parse(&c_spec) {
                        d.to_hex()
                    } else {
                        anyhow::bail!("Cannot resolve commit or branch '{}'", c_spec);
                    }
                } else {
                    println!(
                        "Auto-ingesting '{}' to obtain base tree...",
                        canonical_lower.display()
                    );
                    let ingest_res =
                        graft::git::ingest_directory(&canonical_lower, store.as_ref())?;
                    ingest_res.root_digest.to_hex()
                };

                let mut config =
                    manager.prepare_config(&id, &base_tree, &canonical_lower, &target_dir);
                config.branch = tracked_branch.clone();
                config.base_commit = tracked_base_commit;
                let actual_agent_id = get_default_agent_id(agent_id);

                if !no_lease {
                    let lease_base_digest = if let Some(bc) = &config.base_commit {
                        Digest::parse(bc).unwrap_or_else(|_| {
                            Digest::parse(&base_tree)
                                .unwrap_or_else(|_| Digest::from_bytes(b"root"))
                        })
                    } else {
                        Digest::parse(&base_tree).unwrap_or_else(|_| Digest::from_bytes(b"root"))
                    };
                    let lease = meta_store.acquire_lease(
                        &id,
                        &actual_agent_id,
                        &lease_base_digest,
                        lease_ttl,
                    )?;
                    println!(
                        "✓ Acquired workspace lease: agent '{}', expires at {} ({}s TTL)",
                        lease.agent_id, lease.lease_expires_at, lease_ttl
                    );
                    config.agent_id = Some(actual_agent_id);
                }

                manager.init_workspace_dirs(&config)?;
                manager.save_config(&config)?;

                println!("✓ Workspace initialized for task: {}", id);
                if let Some(b) = &tracked_branch {
                    println!("  Branch:       {}", b);
                }
                println!("  Base tree:    {}", config.commit);
                println!("  Lower (Base): {}", config.lower_dir.display());
                println!("  Upper (CoW):  {}", config.upper_dir.display());
                println!("  Workdir:      {}", config.work_dir.display());
                println!("  Target:       {}", config.merged_dir.display());
                println!();
                println!("To mount OverlayFS, run:");
                println!(
                    "  mount -t overlay overlay -o lowerdir={},upperdir={},workdir={} {}",
                    config.lower_dir.display(),
                    config.upper_dir.display(),
                    config.work_dir.display(),
                    config.merged_dir.display()
                );
            }
            WorkspaceCommands::Status { id, storage_dir, storage } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let config = manager.load_config(&id)?;
                let store = storage.open_store().ok();
                let changes = manager.scan_changes_with_store(&config, store.as_deref())?;

                println!("Workspace changes for '{}': ({} files)", id, changes.len());
                for change in changes {
                    println!("  {:?}: {}", change.change_type, change.relative_path.display());
                }
            }
            WorkspaceCommands::Mount {
                id,
                target,
                daemon,
                internal_daemon,
                backend,
                storage_dir,
                storage,
            } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let mut config = manager.load_config(&id)?;

                let target_dir = target.unwrap_or_else(|| config.merged_dir.clone());
                let canon_target = safe_canonicalize(&target_dir);
                fs::create_dir_all(&canon_target)?;

                if WorkspaceManager::is_mounted(&canon_target) {
                    println!(
                        "Workspace '{}' is already mounted at: {}",
                        id,
                        canon_target.display()
                    );
                    return Ok(());
                }

                // Decide backend
                let is_root = unsafe { libc::geteuid() == 0 };
                let use_overlay = backend == "overlay" || (backend == "auto" && is_root);

                if use_overlay {
                    if !is_root {
                        anyhow::bail!("Kernel OverlayFS requires root privileges (UID 0). Use '--backend fuse' or run without root for unprivileged user-space CoW FUSE.");
                    }
                    let opts = format!(
                        "lowerdir={},upperdir={},workdir={}",
                        config.lower_dir.display(),
                        config.upper_dir.display(),
                        config.work_dir.display()
                    );
                    let mut cmd = std::process::Command::new("mount");
                    cmd.arg("-t")
                        .arg("overlay")
                        .arg("overlay")
                        .arg("-o")
                        .arg(&opts)
                        .arg(&canon_target);
                    let status = cmd.status()?;
                    if !status.success() {
                        anyhow::bail!("Kernel OverlayFS mount failed with status {}", status);
                    }
                    config.mount_backend = Some("overlay".to_string());
                    config.mount_target = Some(canon_target.clone());
                    config.mount_pid = None;
                    manager.save_config(&config)?;
                    println!(
                        "✓ Mounted workspace '{}' via kernel OverlayFS at: {}",
                        id,
                        canon_target.display()
                    );
                    return Ok(());
                }

                // FUSE backend:
                if daemon && !internal_daemon {
                    let exe = std::env::current_exe()?;
                    let mut child_cmd = std::process::Command::new(exe);
                    child_cmd.arg("workspace").arg("mount").arg(&id);
                    child_cmd.arg("--target").arg(&canon_target);
                    child_cmd.arg("--internal-daemon");
                    child_cmd.arg("--storage-dir").arg(&storage_dir);
                    if storage.local {
                        child_cmd.arg("--local");
                    } else {
                        if let Some(r) = storage.resolved_remote() {
                            child_cmd.arg("--remote").arg(r);
                        }
                        if let Some(sp) = storage.resolved_spanner_database() {
                            child_cmd.arg("--spanner-database").arg(sp);
                        }
                    }
                    child_cmd.arg("--cas-dir").arg(storage.resolved_cas_dir());

                    let pid_file = std::env::temp_dir().join(format!(
                        "graft_ws_{}.pid",
                        blake3::hash(canon_target.to_string_lossy().as_bytes()).to_hex()
                    ));
                    let log_path = std::env::temp_dir().join(format!(
                        "graft_ws_{}.log",
                        blake3::hash(canon_target.to_string_lossy().as_bytes()).to_hex()
                    ));
                    let log_file = std::fs::File::create(&log_path)?;
                    child_cmd.stdin(std::process::Stdio::null());
                    child_cmd.stdout(log_file.try_clone()?);
                    child_cmd.stderr(log_file);

                    let child = child_cmd.spawn()?;
                    let pid = child.id();
                    std::fs::write(&pid_file, pid.to_string())?;

                    config.mount_pid = Some(pid);
                    config.mount_backend = Some("fuse".to_string());
                    config.mount_target = Some(canon_target.clone());
                    manager.save_config(&config)?;

                    let start = std::time::Instant::now();
                    let mut mounted = false;
                    while start.elapsed() < std::time::Duration::from_secs(3) {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        if WorkspaceManager::is_mounted(&canon_target) {
                            mounted = true;
                            break;
                        }
                    }

                    if mounted {
                        println!("Graft writeable workspace mount started in background:");
                        println!("  Workspace:  {}", id);
                        println!("  Mountpoint: {}", canon_target.display());
                        println!("  Backend:    FUSE CoW (unprivileged user-space)");
                        println!("  PID:        {}", pid);
                        println!("  Log:        {}", log_path.display());
                        println!("  To unmount: graft workspace unmount {}", id);
                    } else {
                        let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
                        anyhow::bail!(
                            "Mount daemon (PID {}) did not become active within 3s. Log: {}",
                            pid,
                            log_content
                        );
                    }
                    return Ok(());
                }

                #[cfg(feature = "fuse")]
                {
                    let blob_store = storage.open_store()?;
                    let meta_store = storage.open_metadata()?;

                    // Resolve base tree from config.commit
                    let (tree_digest, commit_time) = if let Ok((_, _, commit_obj)) =
                        resolve_revision(&config.commit, blob_store.as_ref(), meta_store.as_ref())
                    {
                        let time = std::time::Duration::from_secs(
                            commit_obj.timestamp_seconds.max(0) as u64,
                        );
                        (commit_obj.root_tree_digest, time)
                    } else if let Ok(d) = Digest::parse(&config.commit) {
                        (d, std::time::Duration::from_secs(1_767_225_600))
                    } else {
                        let ingest_res =
                            graft::git::ingest_directory(&config.lower_dir, blob_store.as_ref())?;
                        (ingest_res.root_digest, std::time::Duration::from_secs(1_767_225_600))
                    };

                    // Prune upper files that are identical to tree_digest before mounting
                    let _ = manager.prune_clean_upper_files(&config, blob_store.as_ref());

                    let tree = graft::vfs::VfsTree::from_cas(&tree_digest, blob_store.as_ref())?;
                    let fs = graft::vfs::GraftWorkspaceFilesystem::from_tree(
                        tree,
                        Arc::clone(&blob_store),
                        &config.upper_dir,
                    )
                    .with_commit_time(commit_time);

                    let session = graft::vfs::spawn_mount_workspace_fuse(fs, &canon_target, None)
                        .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to mount workspace FUSE at {}: {}",
                            canon_target.display(),
                            e
                        )
                    })?;

                    config.mount_backend = Some("fuse".to_string());
                    config.mount_target = Some(canon_target.clone());
                    manager.save_config(&config)?;

                    if !internal_daemon {
                        println!(
                            "Graft writeable workspace mounted at: {}",
                            canon_target.display()
                        );
                        println!("  Workspace: {}", id);
                        println!("  Base tree: {}", tree_digest);
                        println!("  Upper CoW: {}", config.upper_dir.display());
                        println!("Press Ctrl-C (SIGINT) to unmount.");
                    }

                    // Wait for termination signal
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(async {
                        use tokio::signal::unix::{signal, SignalKind};
                        let mut sigterm = signal(SignalKind::terminate()).ok();
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {},
                            _ = async {
                                if let Some(ref mut st) = sigterm {
                                    st.recv().await;
                                } else {
                                    std::future::pending::<()>().await;
                                }
                            } => {},
                        }
                    });

                    if !internal_daemon {
                        println!("\nUnmounting workspace cleanly...");
                    }
                    drop(session);
                    if !internal_daemon {
                        println!("Unmounted: {}", canon_target.display());
                    }
                    config.mount_pid = None;
                    config.mount_backend = None;
                    let _ = manager.save_config(&config);
                }

                #[cfg(not(feature = "fuse"))]
                {
                    anyhow::bail!("This 'graft' binary was built without FUSE support. Rebuild with: cargo build --features fuse");
                }
            }
            WorkspaceCommands::Unmount { id, lazy, storage_dir } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let mut config = match manager.find_workspace(&id) {
                    Ok(c) => c,
                    Err(e) => {
                        let path = PathBuf::from(&id);
                        if WorkspaceManager::is_mounted(&path) {
                            let mut conf = manager.prepare_config(&id, "unknown", &path, &path);
                            conf.mount_target = Some(path);
                            conf
                        } else {
                            return Err(anyhow::anyhow!(e));
                        }
                    }
                };
                let actual_id = config.id.clone();
                manager.unmount_workspace(&mut config, lazy)?;
                println!("✓ Workspace '{}' unmounted cleanly.", actual_id);
            }
            WorkspaceCommands::List { storage_dir } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let workspaces = manager.list_workspaces()?;
                if workspaces.is_empty() {
                    println!("No workspaces found in '{}'.", storage_dir.display());
                    return Ok(());
                }
                println!("Active Workspaces: ({} found)", workspaces.len());
                for ws in workspaces {
                    let mount_target = ws.mount_target.as_ref().unwrap_or(&ws.merged_dir);
                    let is_mounted = WorkspaceManager::is_mounted(mount_target);
                    let status_str = if is_mounted {
                        format!("Mounted at {}", mount_target.display())
                    } else {
                        "Not mounted".to_string()
                    };
                    let branch_str = ws.branch.as_deref().unwrap_or("-");
                    let agent_str = ws.agent_id.as_deref().unwrap_or("-");
                    println!(
                        "  [{}] branch: {}, agent: {}, base: {}, status: {}",
                        ws.id,
                        branch_str,
                        agent_str,
                        &ws.commit[..ws.commit.len().min(12)],
                        status_str
                    );
                }
            }
            WorkspaceCommands::Destroy { id, agent_id, force, storage_dir, storage } => {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let config = match manager.find_workspace(&id) {
                    Ok(c) => c,
                    Err(e) => {
                        let path = PathBuf::from(&id);
                        if WorkspaceManager::is_mounted(&path) {
                            println!("Found active mount at '{}', unmounting...", path.display());
                            let mut conf = manager.prepare_config(&id, "unknown", &path, &path);
                            conf.mount_target = Some(path);
                            conf
                        } else {
                            return Err(anyhow::anyhow!(e));
                        }
                    }
                };
                let actual_id = config.id.clone();
                manager.cleanup(&config)?;
                println!("✓ Workspace '{}' cleaned up successfully.", actual_id);

                if let Ok(meta_store) = storage.open_metadata() {
                    let agent_to_release =
                        agent_id.or(config.agent_id).unwrap_or_else(|| get_default_agent_id(None));
                    if let Err(e) = meta_store.release_lease(&actual_id, &agent_to_release, force) {
                        eprintln!("Notice: could not release lease for '{}': {}", actual_id, e);
                    } else {
                        println!("✓ Released workspace lease for '{}'", actual_id);
                    }
                }
            }
        },
        Commands::Cas { action } => match action {
            CasCommands::Hash { file } => {
                if !file.exists() {
                    anyhow::bail!("File '{}' does not exist.", file.display());
                }
                let content = fs::read(&file)?;
                let digest = Digest::from_bytes(&content);
                println!("{}", digest);
            }
            CasCommands::Put { file, storage } => {
                if !file.exists() {
                    anyhow::bail!("File '{}' does not exist.", file.display());
                }
                let store = storage.open_store()?;
                let content = fs::read(&file)?;
                let digest = store.put(&content)?;
                println!("✓ Stored in CAS: {}", digest);
                if let Some(r) = storage.resolved_remote() {
                    println!("  Uploaded to remote: {}", r);
                }
            }
            CasCommands::Pull { digest, storage } => {
                let store = storage.open_store()?;
                let raw_hex = digest.split(':').next().unwrap_or(&digest);
                let d = Digest::from_hex(raw_hex, 0)
                    .map_err(|e| anyhow::anyhow!("Invalid digest hex: {}", e))?;
                if let Some(bytes) = store.get(&d)? {
                    println!(
                        "✓ Successfully hydrated blob {} ({} bytes) into local cache.",
                        d,
                        bytes.len()
                    );
                } else {
                    anyhow::bail!("Blob {} not found in CAS storage.", d);
                }
            }
            CasCommands::Push { digest, storage } => {
                if let Some(remote_url) = storage.resolved_remote() {
                    let local = LocalBlobStore::new(storage.resolved_cas_dir())?;
                    let raw_hex = digest.split(':').next().unwrap_or(&digest);
                    let d = Digest::from_hex(raw_hex, 0)
                        .map_err(|e| anyhow::anyhow!("Invalid digest hex: {}", e))?;
                    if let Some(bytes) = local.get(&d)? {
                        let remote = GcsBlobStore::new(&remote_url)?;
                        remote.put(&bytes)?;
                        println!(
                            "✓ Successfully pushed blob {} ({} bytes) to {}",
                            d,
                            bytes.len(),
                            remote_url
                        );
                    } else {
                        anyhow::bail!("Blob {} not found in local CAS cache.", d);
                    }
                } else {
                    anyhow::bail!("No remote specified. Pass --remote gs://<bucket>/<prefix>");
                }
            }
        },
        Commands::Ingest { path, storage } => {
            if !path.exists() {
                anyhow::bail!(
                    "Directory '{}' does not exist. Please specify an existing path.",
                    path.display()
                );
            }
            if !path.is_dir() {
                anyhow::bail!("Path '{}' is a file, not a directory.", path.display());
            }

            let store = storage.open_store()?;
            if let Some(remote) = storage.resolved_remote() {
                println!(
                    "Ingesting '{}' into CAS (Local L1: '{}', Remote L2: '{}')...",
                    path.display(),
                    storage.resolved_cas_dir().display(),
                    remote
                );
            } else {
                println!(
                    "Ingesting '{}' into local CAS at '{}'...",
                    path.display(),
                    storage.resolved_cas_dir().display()
                );
            }

            let result = graft::git::ingest_directory(&path, store.as_ref())?;
            println!("✓ Ingestion complete!");
            println!("  Root Digest: {}", result.root_digest);
            println!("  Total files: {}", result.total_files);
            println!("  Total bytes: {} bytes", result.total_bytes);
        }
        Commands::Commit {
            message,
            workspace,
            branch,
            author,
            base_tree,
            parent,
            allow_empty,
            storage_dir,
            storage,
        } => {
            let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
            let mut config = manager.load_config(&workspace).unwrap_or_else(|_| {
                manager.prepare_config(
                    &workspace,
                    "unknown",
                    &PathBuf::from("/dev/null"),
                    &PathBuf::from("/dev/null"),
                )
            });

            if !config.upper_dir.exists() {
                anyhow::bail!(
                    "Workspace '{}' not found at '{}'. Run 'graft workspace create' first.",
                    workspace,
                    config.upper_dir.display()
                );
            }

            let meta_store = storage.open_metadata()?;

            // Verify workspace lease fencing if metadata store configured
            if let Ok(Some(lease)) = meta_store.get_lease(&workspace) {
                let agent_holding = &lease.agent_id;
                let my_agent = config.agent_id.as_deref().unwrap_or("unknown");
                if lease.is_active() && agent_holding != my_agent {
                    anyhow::bail!(
                        "Lease violation: workspace '{}' is currently held by '{}' until {}! Aborting commit to prevent split-brain conflict.",
                        workspace, agent_holding, lease.lease_expires_at
                    );
                }
            }

            let store = storage.open_store()?;
            let changes = manager.scan_changes_with_store(&config, Some(store.as_ref()))?;
            if changes.is_empty() && !allow_empty {
                println!("Nothing to commit: workspace '{}' is clean. (Pass --allow-empty to record empty commit)", workspace);
                return Ok(());
            }

            let base_tree_str = base_tree.unwrap_or(config.commit.clone());
            if base_tree_str.is_empty() || base_tree_str == "unknown" {
                anyhow::bail!("No base tree specified and workspace has no saved base tree. Pass --base-tree <digest>");
            }

            let base_root_digest = Digest::parse(&base_tree_str)
                .map_err(|e| anyhow::anyhow!("Invalid base-tree digest: {}", e))?;

            println!("Creating commit for workspace '{}'...", workspace);
            let new_root_digest = if changes.is_empty() {
                base_root_digest
            } else {
                println!("Applying {} file changes to Merkle tree...", changes.len());
                apply_changes_to_tree(&base_root_digest, &changes, store.as_ref())?
            };

            // 2. Determine parent commits
            let target_branch = branch.or(config.branch.clone());
            let mut resolved_canon_ref = None;
            let parent_commits = if let Some(p_str) = parent {
                vec![Digest::parse(&p_str)
                    .map_err(|e| anyhow::anyhow!("Invalid parent digest: {}", e))?]
            } else if let Some(b_name) = &target_branch {
                if let Ok(Some((canon, head_digest))) = resolve_ref(meta_store.as_ref(), b_name) {
                    resolved_canon_ref = Some(canon);
                    vec![head_digest]
                } else if let Some(bc) = &config.base_commit {
                    vec![Digest::parse(bc)
                        .map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
                } else {
                    vec![]
                }
            } else if let Some(bc) = &config.base_commit {
                vec![Digest::parse(bc).map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
            } else {
                vec![]
            };

            let commit =
                Commit::new(parent_commits.clone(), new_root_digest, author.clone(), message);
            let commit_bytes = commit.serialize();
            let commit_digest = store.put(&commit_bytes)?;

            // Save commit metadata in metadata store
            meta_store.put_commit_metadata(&commit_digest, &commit)?;

            // Advance branch ref atomically if branch is configured
            if let Some(b_name) = &target_branch {
                let canon_ref = resolved_canon_ref.unwrap_or_else(|| normalize_ref_name(b_name));
                let expected = parent_commits.first();
                meta_store.compare_and_swap_ref(&canon_ref, expected, &commit_digest, &author)?;
                println!("  Branch:        '{}' advanced -> {}", canon_ref, commit_digest);
                config.branch = Some(canon_ref);
            }

            config.base_commit = Some(commit_digest.to_string());
            config.commit = new_root_digest.to_string();
            let _ = manager.save_config(&config);

            let stats_opt = if let Some(parent_commit_digest) = parent_commits.first() {
                let parent_tree =
                    graft::cas::get_commit_tree_digest(parent_commit_digest, store.as_ref());
                diff_trees(&parent_tree, &new_root_digest, store.as_ref())
                    .ok()
                    .map(|d| compute_diff_stats(&d))
            } else {
                None
            };

            let stats_suffix = if let Some(stats) = stats_opt {
                format!(" (+{} lines, -{} lines)", stats.additions, stats.deletions)
            } else {
                String::new()
            };

            println!("✓ Commit successful!");
            println!("  Commit Digest: {}", commit_digest);
            println!("  Root Tree:     {}", new_root_digest);
            if !parent_commits.is_empty() {
                let p_str =
                    parent_commits.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ");
                println!("  Parent(s):     {}", p_str);
            }
            println!("  Files changed: {}{}", changes.len(), stats_suffix);
            for change in changes {
                println!("    {:?}: {}", change.change_type, change.relative_path.display());
            }
        }
        Commands::Diff { workspace, lower, base, target, storage_dir, storage } => {
            let store = storage.open_store()?;

            if let (Some(b_hex), Some(t_hex)) = (&base, &target) {
                let b_digest = Digest::parse(b_hex)
                    .map_err(|e| anyhow::anyhow!("Invalid base digest: {}", e))?;
                let t_digest = Digest::parse(t_hex)
                    .map_err(|e| anyhow::anyhow!("Invalid target digest: {}", e))?;

                let diff_output = diff_trees(&b_digest, &t_digest, store.as_ref())?;
                if diff_output.is_empty() {
                    println!("Trees are identical.");
                } else {
                    print!("{}", diff_output);
                }
            } else if let Some(ws_id) = &workspace {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                let config = if let Ok(saved) = manager.load_config(ws_id) {
                    saved
                } else {
                    let lower_path = lower.unwrap_or_else(|| PathBuf::from("."));
                    manager.prepare_config(
                        ws_id,
                        "unknown",
                        &lower_path,
                        &PathBuf::from("/dev/null"),
                    )
                };

                if !config.upper_dir.exists() {
                    anyhow::bail!(
                        "Workspace '{}' not found at '{}'. Run 'graft workspace create' first.",
                        ws_id,
                        config.upper_dir.display()
                    );
                }

                let diff_output = diff_workspace(&config, store.as_ref())?;
                if diff_output.is_empty() {
                    println!("No changes in workspace '{}'.", ws_id);
                } else {
                    print!("{}", diff_output);
                }
            } else {
                anyhow::bail!(
                    "Specify either --workspace <id> or both --base <digest> and --target <digest>."
                );
            }
        }
        Commands::Log { commit, limit, storage } => {
            let store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;
            let (ref_opt, head_digest, _) =
                resolve_revision(&commit, store.as_ref(), meta_store.as_ref())?;

            if let Some(r) = ref_opt {
                println!("Commit log for branch '{}' (HEAD -> {}):", r, head_digest);
            } else {
                println!("Commit log for {}:", head_digest);
            }
            println!();

            let history = get_commit_history(&head_digest, limit, store.as_ref())?;
            if history.is_empty() {
                println!("No commit history found for {}", commit);
                return Ok(());
            }

            for (digest, c) in history {
                println!("commit {}", digest.to_hex());
                println!("Author: {}", c.author);
                println!("Date:   {} (Unix timestamp)", c.timestamp_seconds);
                println!("Tree:   {}", c.root_tree_digest);
                if !c.parent_commits.is_empty() {
                    let parents =
                        c.parent_commits.iter().map(|p| p.to_hex()).collect::<Vec<_>>().join(" ");
                    println!("Parents: {}", parents);
                    if let Some(parent_commit_digest) = c.parent_commits.first() {
                        let parent_tree =
                            graft::cas::get_commit_tree_digest(parent_commit_digest, store.as_ref());
                        if let Ok(diff) =
                            diff_trees(&parent_tree, &c.root_tree_digest, store.as_ref())
                        {
                            let stats = compute_diff_stats(&diff);
                            if stats.files_changed > 0 {
                                println!(
                                    "Stats:   +{} / -{} lines ({} {} changed)",
                                    stats.additions,
                                    stats.deletions,
                                    stats.files_changed,
                                    if stats.files_changed == 1 { "file" } else { "files" }
                                );
                            }
                        }
                    }
                }
                println!();
                for line in c.message.lines() {
                    println!("    {}", line);
                }
                println!();
            }
        }
        Commands::Export { action } => match action {
            ExportCommands::Patch {
                workspace,
                lower,
                base,
                target,
                output,
                storage_dir,
                storage,
            } => {
                let store = storage.open_store()?;

                let diff_output = if let (Some(b_hex), Some(t_hex)) = (&base, &target) {
                    let b_digest = Digest::parse(b_hex)
                        .map_err(|e| anyhow::anyhow!("Invalid base digest: {}", e))?;
                    let t_digest = Digest::parse(t_hex)
                        .map_err(|e| anyhow::anyhow!("Invalid target digest: {}", e))?;
                    diff_trees(&b_digest, &t_digest, store.as_ref())?
                } else if let Some(ws_id) = &workspace {
                    let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                    let config = if let Ok(saved) = manager.load_config(ws_id) {
                        saved
                    } else {
                        let lower_path = lower.unwrap_or_else(|| PathBuf::from("."));
                        manager.prepare_config(
                            ws_id,
                            "unknown",
                            &lower_path,
                            &PathBuf::from("/dev/null"),
                        )
                    };

                    if !config.upper_dir.exists() {
                        anyhow::bail!(
                            "Workspace '{}' not found at '{}'.",
                            ws_id,
                            config.upper_dir.display()
                        );
                    }
                    diff_workspace(&config, store.as_ref())?
                } else {
                    anyhow::bail!("Specify either --workspace <id> or both --base and --target.");
                };

                if let Some(out_path) = output {
                    export_patch_file(&diff_output, &out_path)?;
                    println!("✓ Patch exported to {}", out_path.display());
                } else {
                    print!("{}", diff_output);
                }
            }
            ExportCommands::Tree { tree, target, storage } => {
                let store = storage.open_store()?;
                let root_digest = Digest::parse(&tree)
                    .map_err(|e| anyhow::anyhow!("Invalid tree digest: {}", e))?;

                println!("Materializing tree {} into '{}'...", root_digest, target.display());
                let count = export_tree_to_dir(&root_digest, &target, store.as_ref())?;
                println!("✓ Materialized {} files into '{}'.", count, target.display());
            }
        },
        Commands::Merge {
            source,
            into,
            ours,
            theirs,
            base,
            message,
            author,
            no_ff,
            allow_conflicts,
            workspace,
            storage_dir,
            storage,
        } => {
            let store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;

            let theirs_spec = source.or(theirs).ok_or_else(|| {
                anyhow::anyhow!("Specify source branch or commit to merge: graft merge <source>")
            })?;

            let ours_spec = if let Some(i) = into {
                i
            } else if let Some(o) = ours {
                o
            } else {
                let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));
                if let Ok(config) = manager.load_config(&workspace) {
                    if let Some(b) = config.branch {
                        b
                    } else {
                        "main".to_string()
                    }
                } else {
                    "main".to_string()
                }
            };

            let (_theirs_ref, theirs_commit_digest, theirs_commit) =
                resolve_revision(&theirs_spec, store.as_ref(), meta_store.as_ref())?;

            let max_attempts = 3;
            for attempt in 0..max_attempts {
                let (ours_ref, ours_commit_digest, ours_commit) =
                    resolve_revision(&ours_spec, store.as_ref(), meta_store.as_ref())?;

                if ours_commit_digest == theirs_commit_digest {
                    println!("Already up to date.");
                    return Ok(());
                }

                let common_ancestor_opt = find_common_ancestor(
                    &ours_commit_digest,
                    &theirs_commit_digest,
                    store.as_ref(),
                )?;

                if common_ancestor_opt == Some(theirs_commit_digest) {
                    println!(
                        "Already up to date: '{}' already includes '{}'.",
                        ours_spec, theirs_spec
                    );
                    return Ok(());
                }

                // Fast-Forward Path
                if common_ancestor_opt == Some(ours_commit_digest) && !no_ff {
                    println!(
                        "Fast-forwarding '{}' from {} to {}...",
                        ours_spec, ours_commit_digest, theirs_commit_digest
                    );
                    if let Some(target_ref) = &ours_ref {
                        match meta_store.compare_and_swap_ref(
                            target_ref,
                            Some(&ours_commit_digest),
                            &theirs_commit_digest,
                            &author,
                        ) {
                            Ok(_) => {
                                println!(
                                    "✓ Fast-forward merge succeeded: '{}' updated to {}.",
                                    target_ref, theirs_commit_digest
                                );
                                return Ok(());
                            }
                            Err(_e) if attempt + 1 < max_attempts => {
                                println!("Notice: branch '{}' updated concurrently; retrying fast-forward (attempt {}/{})...", target_ref, attempt + 2, max_attempts);
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                continue;
                            }
                            Err(e) => anyhow::bail!(
                                "Concurrency conflict updating branch '{}': {}",
                                target_ref,
                                e
                            ),
                        }
                    } else {
                        println!("✓ Fast-forward target commit: {}", theirs_commit_digest);
                        return Ok(());
                    }
                }

                // 3-Way Merkle Tree Merge Path
                let base_tree_digest = if let Some(base_str) = &base {
                    let base_digest = Digest::parse(base_str)
                        .map_err(|e| anyhow::anyhow!("Invalid --base digest: {}", e))?;
                    if let Some(bytes) = store.get(&base_digest)? {
                        if let Ok(c) = Commit::deserialize(&bytes) {
                            c.root_tree_digest
                        } else {
                            base_digest
                        }
                    } else {
                        base_digest
                    }
                } else if let Some(ancestor_digest) = common_ancestor_opt {
                    let bytes = store.get(&ancestor_digest)?.unwrap();
                    let ancestor_commit = Commit::deserialize(&bytes)
                        .map_err(|e| anyhow::anyhow!("Failed to parse ancestor: {}", e))?;
                    println!("Found common ancestor commit: {}", ancestor_digest);
                    ancestor_commit.root_tree_digest
                } else {
                    println!("Warning: No common ancestor found. Merging unrelated histories.");
                    Digest::from_bytes(b"empty")
                };

                println!("Merging Merkle trees in O(changed)...");
                let merge_result = merge_trees(
                    &base_tree_digest,
                    &ours_commit.root_tree_digest,
                    &theirs_commit.root_tree_digest,
                    store.as_ref(),
                )?;

                if merge_result.has_conflicts {
                    println!(
                        "⚠ Merge encountered {} conflicting files:",
                        merge_result.conflict_files.len()
                    );
                    for file in &merge_result.conflict_files {
                        println!("  CONFLICT: {}", file);
                    }
                    println!("Conflicted Tree: {}", merge_result.root_tree_digest);
                    if !allow_conflicts {
                        anyhow::bail!(
                            "Merge aborted due to conflicts. Pass --allow-conflicts to record conflict markers."
                        );
                    }
                } else {
                    println!("✓ Automatic 3-way merge succeeded with 0 conflicts!");
                    println!("Merged files ({}):", merge_result.merged_files.len());
                    for file in &merge_result.merged_files {
                        println!("  MERGED: {}", file);
                    }
                }

                // Create merge commit with 2 parents: [ours, theirs]
                let default_msg = format!("Merge branch '{}' into '{}'", theirs_spec, ours_spec);
                let commit_msg = message.clone().unwrap_or(default_msg);

                let merge_commit = Commit::new(
                    vec![ours_commit_digest, theirs_commit_digest],
                    merge_result.root_tree_digest,
                    author.clone(),
                    commit_msg,
                );
                let commit_bytes = merge_commit.serialize();
                let merge_commit_digest = store.put(&commit_bytes)?;

                let _ = meta_store.put_commit_metadata(&merge_commit_digest, &merge_commit);

                // Advance target branch ref if target is a branch reference
                if let Some(target_ref) = &ours_ref {
                    match meta_store.compare_and_swap_ref(
                        target_ref,
                        Some(&ours_commit_digest),
                        &merge_commit_digest,
                        &author,
                    ) {
                        Ok(_) => {
                            println!();
                            println!("Merge Commit: {}", merge_commit_digest);
                            println!("Root Tree:    {}", merge_result.root_tree_digest);
                            println!(
                                "Parents:      {} {}",
                                ours_commit_digest, theirs_commit_digest
                            );
                            println!(
                                "Branch:       '{}' updated -> {}",
                                target_ref, merge_commit_digest
                            );
                            return Ok(());
                        }
                        Err(_e) if attempt + 1 < max_attempts => {
                            println!("Target branch '{}' changed concurrently; retrying optimistic merge (attempt {}/{})...", target_ref, attempt + 2, max_attempts);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        }
                        Err(e) => anyhow::bail!(
                            "Failed to update branch '{}' after optimistic merge retries: {}",
                            target_ref,
                            e
                        ),
                    }
                } else {
                    println!();
                    println!("Merge Commit: {}", merge_commit_digest);
                    println!("Root Tree:    {}", merge_result.root_tree_digest);
                    println!("Parents:      {} {}", ours_commit_digest, theirs_commit_digest);
                    return Ok(());
                }
            }

            anyhow::bail!("Merge failed due to persistent concurrent conflicts on '{}'", ours_spec);
        }
        Commands::CherryPick {
            commit,
            workspace,
            branch,
            no_commit,
            message,
            author,
            mainline,
            strategy,
            abort,
            r#continue,
            storage_dir,
            storage,
        } => {
            let store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;
            let manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));

            let target_ws_id = if branch.is_none() {
                Some(workspace.clone().unwrap_or_else(|| "default".to_string()))
            } else {
                workspace.clone()
            };

            // 1. Handle --abort
            if abort {
                let ws_id = target_ws_id.ok_or_else(|| {
                    anyhow::anyhow!("Specify --workspace <id> to abort an in-progress cherry-pick")
                })?;
                let state_opt = manager.load_cherry_pick_state(&ws_id)?;
                let state = match state_opt {
                    Some(s) => s,
                    None => anyhow::bail!("No cherry-pick in progress for workspace '{}'.", ws_id),
                };

                let config = manager.load_config(&ws_id)?;
                let mut rolled_back = 0;
                for rel_path in &state.applied_files {
                    let disk_path = config.upper_dir.join(rel_path);
                    if disk_path.exists() {
                        if disk_path.is_file() {
                            let _ = fs::remove_file(&disk_path);
                            rolled_back += 1;
                        } else if disk_path.is_dir() {
                            let _ = fs::remove_dir_all(&disk_path);
                            rolled_back += 1;
                        }
                    }
                    if let Some(file_name) = Path::new(rel_path).file_name() {
                        let parent = Path::new(rel_path).parent().unwrap_or_else(|| Path::new(""));
                        let wh_name = format!(".wh.{}", file_name.to_string_lossy());
                        let wh_disk_path = config.upper_dir.join(parent).join(wh_name);
                        if wh_disk_path.exists() {
                            let _ = fs::remove_file(&wh_disk_path);
                            rolled_back += 1;
                        }
                    }
                }

                manager.clear_cherry_pick_state(&ws_id)?;
                println!("✓ Cherry-pick aborted for workspace '{}'.", ws_id);
                println!("  Rolled back {} file modification(s). Workspace restored.", rolled_back);
                return Ok(());
            }

            // 2. Handle --continue
            if r#continue {
                let ws_id = target_ws_id.ok_or_else(|| {
                    anyhow::anyhow!("Specify --workspace <id> to continue a cherry-pick")
                })?;
                let state_opt = manager.load_cherry_pick_state(&ws_id)?;
                let state = match state_opt {
                    Some(s) => s,
                    None => anyhow::bail!(
                        "No cherry-pick in progress for workspace '{}'. Run 'graft cherry-pick <commit>' first.",
                        ws_id
                    ),
                };

                let mut config = manager.load_config(&ws_id)?;

                let mut unresolved = Vec::new();
                for rel_path in &state.conflict_files {
                    let disk_path = config.upper_dir.join(rel_path);
                    if disk_path.exists() {
                        if let Ok(bytes) = fs::read(&disk_path) {
                            if let Ok(content) = std::str::from_utf8(&bytes) {
                                if content.contains("<<<<<<< ours")
                                    || content.contains("=======")
                                    || content.contains(">>>>>>> theirs")
                                {
                                    unresolved.push(rel_path.clone());
                                }
                            }
                        }
                    }
                }

                if !unresolved.is_empty() {
                    eprintln!(
                        "error: unresolved conflict markers remain in {} file(s):",
                        unresolved.len()
                    );
                    for path in &unresolved {
                        eprintln!("  - {}", path);
                    }
                    eprintln!();
                    eprintln!("Resolve all conflict markers and run 'graft cherry-pick --continue', or run 'graft cherry-pick --abort' to cancel.");
                    anyhow::bail!("Unresolved merge conflicts remain.");
                }

                let changes = manager.scan_changes(&config)?;
                let base_tree_str = config.commit.clone();
                let base_root_digest = if let Ok((_, _, c)) =
                    resolve_revision(&base_tree_str, store.as_ref(), meta_store.as_ref())
                {
                    c.root_tree_digest
                } else {
                    Digest::parse(&base_tree_str)
                        .map_err(|e| anyhow::anyhow!("Invalid workspace base tree: {}", e))?
                };

                let new_root_digest = if changes.is_empty() {
                    base_root_digest
                } else {
                    apply_changes_to_tree(&base_root_digest, &changes, store.as_ref())?
                };

                let target_branch = state.target_branch.clone().or(config.branch.clone());
                let parent_commits = if let Some(ref b_name) = target_branch {
                    if let Ok(Some((_, head_digest))) = resolve_ref(meta_store.as_ref(), b_name) {
                        vec![head_digest]
                    } else if let Some(ref bc) = config.base_commit {
                        vec![Digest::parse(bc)
                            .map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
                    } else {
                        vec![]
                    }
                } else if let Some(ref bc) = config.base_commit {
                    vec![Digest::parse(bc)
                        .map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
                } else {
                    vec![]
                };

                let commit_msg = message.clone().unwrap_or(state.original_message.clone());
                let commit_author = if author != "Cherry-Pick <cherry-pick@graft.dev>" {
                    author.clone()
                } else {
                    state.original_author.clone()
                };

                let commit_obj = Commit::new(
                    parent_commits.clone(),
                    new_root_digest,
                    commit_author.clone(),
                    commit_msg.clone(),
                );
                let commit_bytes = commit_obj.serialize();
                let new_commit_digest = store.put(&commit_bytes)?;
                meta_store.put_commit_metadata(&new_commit_digest, &commit_obj)?;

                if let Some(ref b_name) = target_branch {
                    let canon_ref = normalize_ref_name(b_name);
                    let expected = parent_commits.first();
                    meta_store.compare_and_swap_ref(
                        &canon_ref,
                        expected,
                        &new_commit_digest,
                        &commit_author,
                    )?;
                    println!("  Branch:        '{}' advanced -> {}", canon_ref, new_commit_digest);
                    config.branch = Some(canon_ref);
                }

                config.base_commit = Some(new_commit_digest.to_string());
                config.commit = new_root_digest.to_string();
                let _ = manager.save_config(&config);
                manager.clear_cherry_pick_state(&ws_id)?;

                println!("✓ Cherry-pick resolved and committed successfully!");
                println!("  Workspace:     {}", ws_id);
                println!("  Commit Digest: {}", new_commit_digest);
                println!("  Root Tree:     {}", new_root_digest);
                println!("  Author:        {}", commit_author);
                println!("  Message:       {}", commit_msg);
                return Ok(());
            }

            // 3. Standard cherry-pick execution: commit argument is required
            let commit_spec = commit.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing commit argument. Usage: graft cherry-pick <commit> [--workspace <id>] [--branch <name>]"
                )
            })?;

            let strat = ConflictStrategy::from_str(strategy.as_str())
                .map_err(|e| anyhow::anyhow!("Invalid strategy '{}': {}", strategy, e))?;

            let (_src_ref, source_commit_digest, _source_commit) =
                resolve_revision(commit_spec, store.as_ref(), meta_store.as_ref())?;

            // Path 1: Direct target branch in CAS
            if let Some(target_branch_name) = branch {
                let (canon_ref_opt, target_head_digest, target_commit) =
                    resolve_revision(&target_branch_name, store.as_ref(), meta_store.as_ref())?;
                let canon_ref =
                    canon_ref_opt.unwrap_or_else(|| normalize_ref_name(&target_branch_name));

                println!(
                    "Cherry-picking {} onto branch '{}' ({}) in CAS...",
                    source_commit_digest, canon_ref, target_head_digest
                );

                let res = cherry_pick_commit(
                    &source_commit_digest,
                    &target_commit.root_tree_digest,
                    mainline,
                    strat,
                    store.as_ref(),
                )?;

                if res.has_conflicts {
                    if strat == ConflictStrategy::Markers || strat == ConflictStrategy::Fail {
                        eprintln!(
                            "error: cherry-pick would result in {} conflict(s) on branch '{}':",
                            res.conflict_files.len(),
                            canon_ref
                        );
                        for f in &res.conflict_files {
                            eprintln!("  CONFLICT: {}", f);
                        }
                        eprintln!();
                        eprintln!(
                            "Cannot apply conflict markers directly to branch '{}' without a working directory.",
                            canon_ref
                        );
                        eprintln!(
                            "Create an ephemeral workspace to resolve conflicts interactively:"
                        );
                        eprintln!("  graft workspace create my-ws --branch {}", target_branch_name);
                        eprintln!("  graft cherry-pick {} --workspace my-ws", commit_spec);
                        anyhow::bail!(
                            "Cherry-pick encountered conflicts on branch '{}'.",
                            canon_ref
                        );
                    }
                }

                let commit_msg = message.clone().unwrap_or(res.original_message.clone());
                let commit_author = if author != "Cherry-Pick <cherry-pick@graft.dev>" {
                    author.clone()
                } else {
                    res.original_author.clone()
                };

                let new_commit = Commit::new(
                    vec![target_head_digest],
                    res.root_tree_digest,
                    commit_author.clone(),
                    commit_msg.clone(),
                );
                let new_commit_bytes = new_commit.serialize();
                let new_commit_digest = store.put(&new_commit_bytes)?;
                meta_store.put_commit_metadata(&new_commit_digest, &new_commit)?;

                meta_store.compare_and_swap_ref(
                    &canon_ref,
                    Some(&target_head_digest),
                    &new_commit_digest,
                    &commit_author,
                )?;

                println!("✓ Cherry-pick successful on branch '{}'!", canon_ref);
                println!("  New Commit:    {}", new_commit_digest);
                println!("  Parent Commit: {}", target_head_digest);
                println!("  Root Tree:     {}", res.root_tree_digest);
                println!("  Author:        {}", commit_author);
                println!("  Message:       {}", commit_msg);
                println!("  Files Merged:  {}", res.merged_files.len());
                return Ok(());
            }

            // Path 2: Target workspace
            let ws_id = target_ws_id.unwrap();
            let mut ws_config = manager.load_config(&ws_id)?;

            if manager.load_cherry_pick_state(&ws_id)?.is_some() {
                anyhow::bail!(
                    "A cherry-pick is already in progress in workspace '{}'.\nResolve conflicts and run 'graft cherry-pick --continue', or 'graft cherry-pick --abort' to discard.",
                    ws_id
                );
            }

            let target_tree_digest = if let Ok((_, _, c)) =
                resolve_revision(&ws_config.commit, store.as_ref(), meta_store.as_ref())
            {
                c.root_tree_digest
            } else if let Ok(d) = Digest::parse(&ws_config.commit) {
                d
            } else {
                anyhow::bail!("Invalid workspace base tree: {}", ws_config.commit);
            };

            println!(
                "Cherry-picking {} onto workspace '{}' (base tree: {})...",
                source_commit_digest, ws_id, target_tree_digest
            );

            let res = cherry_pick_commit(
                &source_commit_digest,
                &target_tree_digest,
                mainline,
                strat,
                store.as_ref(),
            )?;

            let deltas =
                extract_tree_delta(&target_tree_digest, &res.root_tree_digest, store.as_ref())?;
            apply_tree_delta_to_workspace(&ws_config.upper_dir, &deltas, store.as_ref())?;

            let applied_files: Vec<String> = deltas
                .iter()
                .map(|d| match d {
                    TreeDelta::Upsert { path, .. } => path.to_string_lossy().to_string(),
                    TreeDelta::Delete { path } => path.to_string_lossy().to_string(),
                })
                .collect();

            if res.has_conflicts {
                let state = CherryPickState {
                    source_commit: source_commit_digest.to_string(),
                    original_message: message.clone().unwrap_or(res.original_message.clone()),
                    original_author: if author != "Cherry-Pick <cherry-pick@graft.dev>" {
                        author.clone()
                    } else {
                        res.original_author.clone()
                    },
                    conflict_files: res.conflict_files.clone(),
                    applied_files: applied_files.clone(),
                    strategy: strategy.clone(),
                    target_branch: ws_config.branch.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                };
                manager.save_cherry_pick_state(&ws_id, &state)?;

                eprintln!(
                    "CONFLICT (content): Merge conflict in {} file(s):",
                    res.conflict_files.len()
                );
                for f in &res.conflict_files {
                    eprintln!("  CONFLICT: {}", f);
                }
                eprintln!();
                eprintln!("Automatic cherry-pick failed. Three-way conflict markers have been written to:");
                for f in &res.conflict_files {
                    eprintln!("  {}", ws_config.upper_dir.join(f).display());
                }
                eprintln!();
                eprintln!("To finalize after editing conflict markers:");
                eprintln!("  graft cherry-pick --continue --workspace {}", ws_id);
                eprintln!("To abort and restore the workspace:");
                eprintln!("  graft cherry-pick --abort --workspace {}", ws_id);
                return Ok(());
            }

            if no_commit {
                println!(
                    "✓ Cherry-pick applied to workspace '{}' without committing (--no-commit).",
                    ws_id
                );
                println!("  Files modified/created: {}", applied_files.len());
                for f in &applied_files {
                    println!("    {}", f);
                }
                return Ok(());
            }

            let parent_commits = if let Some(ref b_name) = ws_config.branch {
                if let Ok(Some((_, head_digest))) = resolve_ref(meta_store.as_ref(), b_name) {
                    vec![head_digest]
                } else if let Some(ref bc) = ws_config.base_commit {
                    vec![Digest::parse(bc)
                        .map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
                } else {
                    vec![]
                }
            } else if let Some(ref bc) = ws_config.base_commit {
                vec![Digest::parse(bc).map_err(|e| anyhow::anyhow!("Invalid base commit: {}", e))?]
            } else {
                vec![]
            };

            let commit_msg = message.clone().unwrap_or(res.original_message.clone());
            let commit_author = if author != "Cherry-Pick <cherry-pick@graft.dev>" {
                author.clone()
            } else {
                res.original_author.clone()
            };

            let commit_obj = Commit::new(
                parent_commits.clone(),
                res.root_tree_digest,
                commit_author.clone(),
                commit_msg.clone(),
            );
            let commit_bytes = commit_obj.serialize();
            let new_commit_digest = store.put(&commit_bytes)?;
            meta_store.put_commit_metadata(&new_commit_digest, &commit_obj)?;

            if let Some(ref b_name) = ws_config.branch {
                let canon_ref = normalize_ref_name(b_name);
                let expected = parent_commits.first();
                meta_store.compare_and_swap_ref(
                    &canon_ref,
                    expected,
                    &new_commit_digest,
                    &commit_author,
                )?;
                println!("  Branch:        '{}' advanced -> {}", canon_ref, new_commit_digest);
            }

            ws_config.base_commit = Some(new_commit_digest.to_string());
            ws_config.commit = res.root_tree_digest.to_string();
            let _ = manager.save_config(&ws_config);

            println!("✓ Cherry-pick committed successfully!");
            println!("  Workspace:     {}", ws_id);
            println!("  Commit Digest: {}", new_commit_digest);
            println!("  Root Tree:     {}", res.root_tree_digest);
            println!("  Author:        {}", commit_author);
            println!("  Message:       {}", commit_msg);
            println!("  Files applied: {}", applied_files.len());
            for f in &applied_files {
                println!("    {}", f);
            }
        }
        Commands::Branch { name, start_point, from, list, delete, force, storage } => {
            let meta_store = storage.open_metadata()?;
            if delete {
                let ref_name =
                    name.ok_or_else(|| anyhow::anyhow!("Branch name required for --delete"))?;
                let (canon_ref, current) = resolve_ref(meta_store.as_ref(), &ref_name)?
                    .ok_or_else(|| anyhow::anyhow!("Branch '{}' does not exist", ref_name))?;
                meta_store.delete_ref(&canon_ref, if force { None } else { Some(&current) })?;
                println!("✓ Deleted branch '{}' (was {})", canon_ref, current);
            } else if list || (name.is_none() && start_point.is_none() && from.is_none()) {
                let refs = meta_store.list_refs("")?;
                if refs.is_empty() {
                    println!("No branches found.");
                } else {
                    println!("Branches ({}):", refs.len());
                    for (r, d) in refs {
                        let display_name = r.strip_prefix("refs/heads/").unwrap_or(&r);
                        if r.starts_with("refs/heads/") {
                            println!("  {} ({}) -> {}", display_name, r, d);
                        } else {
                            println!("  {} -> {}", r, d);
                        }
                    }
                }
            } else {
                let b_name =
                    name.ok_or_else(|| anyhow::anyhow!("Branch name required to create branch"))?;
                validate_branch_name(&b_name).map_err(|e| anyhow::anyhow!(e))?;
                let canon_ref = normalize_ref_name(&b_name);

                let start_spec = from.or(start_point).unwrap_or_else(|| "main".to_string());
                let store = storage.open_store()?;
                let (_ref_opt, target_digest, _) =
                    resolve_revision(&start_spec, store.as_ref(), meta_store.as_ref())?;

                let current = meta_store.get_ref(&canon_ref)?;
                if current.is_some() && !force {
                    anyhow::bail!(
                        "Branch '{}' already exists at {}. Pass --force to overwrite.",
                        canon_ref,
                        current.unwrap()
                    );
                }
                meta_store.compare_and_swap_ref(
                    &canon_ref,
                    if force { current.as_ref() } else { None },
                    &target_digest,
                    "developer",
                )?;
                println!(
                    "✓ Created branch '{}' -> {} (from '{}')",
                    canon_ref, target_digest, start_spec
                );
            }
        }
        Commands::Lease { action } => match action {
            LeaseCommands::Acquire { workspace, agent_id, commit, ttl_secs, storage } => {
                let meta_store = storage.open_metadata()?;
                let actual_agent = get_default_agent_id(agent_id);
                let base_digest = match commit {
                    Some(c) => Digest::parse(&c).map_err(|e| anyhow::anyhow!(e))?,
                    None => Digest::from_bytes(b"empty"),
                };
                let lease =
                    meta_store.acquire_lease(&workspace, &actual_agent, &base_digest, ttl_secs)?;
                println!("✓ Acquired lease for workspace '{}'", workspace);
                println!("  Agent:      {}", lease.agent_id);
                println!("  Base Tree:  {}", lease.base_commit);
                println!("  Expires At: {} ({}s TTL)", lease.lease_expires_at, ttl_secs);
            }
            LeaseCommands::Heartbeat { workspace, agent_id, ttl_secs, storage } => {
                let meta_store = storage.open_metadata()?;
                let actual_agent = get_default_agent_id(agent_id);
                let lease = meta_store.renew_lease(&workspace, &actual_agent, ttl_secs)?;
                println!("✓ Renewed lease for workspace '{}'", workspace);
                println!("  Agent:      {}", lease.agent_id);
                println!("  Expires At: {} ({}s TTL)", lease.lease_expires_at, ttl_secs);
            }
            LeaseCommands::Release { workspace, agent_id, force, storage } => {
                let meta_store = storage.open_metadata()?;
                let actual_agent = get_default_agent_id(agent_id);
                meta_store.release_lease(&workspace, &actual_agent, force)?;
                println!("✓ Released lease for workspace '{}'", workspace);
            }
            LeaseCommands::Status { workspace, storage } => {
                let meta_store = storage.open_metadata()?;
                if let Some(lease) = meta_store.get_lease(&workspace)? {
                    println!("Workspace Lease: {}", workspace);
                    println!("  Agent:      {}", lease.agent_id);
                    println!("  Base Tree:  {}", lease.base_commit);
                    println!("  Expires At: {}", lease.lease_expires_at);
                    if lease.is_active() {
                        println!("  Status:     ACTIVE ({}s remaining)", lease.remaining_seconds());
                    } else {
                        println!("  Status:     EXPIRED");
                    }
                } else {
                    println!("No active lease found for workspace '{}'.", workspace);
                }
            }
            LeaseCommands::List { storage } => {
                let meta_store = storage.open_metadata()?;
                let leases = meta_store.list_leases()?;
                if leases.is_empty() {
                    println!("No active workspace leases found.");
                } else {
                    println!("Active Workspace Leases ({}):", leases.len());
                    for l in leases {
                        let status_str = if l.is_active() {
                            format!("ACTIVE ({}s remaining)", l.remaining_seconds())
                        } else {
                            "EXPIRED".to_string()
                        };
                        println!(
                            "  {} | Agent: {} | Expires: {} | {}",
                            l.workspace_id, l.agent_id, l.lease_expires_at, status_str
                        );
                    }
                }
            }
        },
        Commands::Mount { mountpoint, commit, daemon, internal_daemon, storage } => {
            let canon_mountpoint = safe_canonicalize(&mountpoint);
            let blob_store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;

            // 1. Resolve commit, branch, or tree digest
            let (tree_digest, commit_time, desc) =
                match resolve_revision(&commit, blob_store.as_ref(), meta_store.as_ref()) {
                    Ok((ref_opt, commit_digest, commit_obj)) => {
                        let name =
                            ref_opt.unwrap_or_else(|| commit_digest.to_hex()[..12].to_string());
                        let time = std::time::Duration::from_secs(
                            commit_obj.timestamp_seconds.max(0) as u64,
                        );
                        (
                            commit_obj.root_tree_digest,
                            time,
                            format!("commit {} ({})", &commit_digest.to_hex()[..12], name),
                        )
                    }
                    Err(_) => {
                        let d = Digest::parse(&commit)
                            .or_else(|_| Digest::from_hex(&commit, 0))
                            .map_err(|_| {
                                anyhow::anyhow!("Could not resolve revision or tree '{}'", commit)
                            })?;
                        if blob_store.has(&d) {
                            (
                                d,
                                std::time::Duration::from_secs(1_767_225_600),
                                format!("tree {}", d),
                            )
                        } else {
                            anyhow::bail!("Tree or commit '{}' not found in CAS", commit);
                        }
                    }
                };
            let _ = (&tree_digest, &commit_time);

            // 2. Ensure mountpoint directory exists
            fs::create_dir_all(&canon_mountpoint)?;

            // 3. Daemon mode: re-spawn current binary with --internal-daemon
            if daemon && !internal_daemon {
                let exe = std::env::current_exe()?;
                let mut child_cmd = std::process::Command::new(exe);
                child_cmd.arg("mount");
                child_cmd.arg(&canon_mountpoint);
                child_cmd.arg("--commit").arg(&commit);
                child_cmd.arg("--internal-daemon");
                if let Some(r) = storage.resolved_remote() {
                    child_cmd.arg("--remote").arg(r);
                }
                child_cmd.arg("--cas-dir").arg(storage.resolved_cas_dir());
                if let Some(sp) = &storage.spanner_database {
                    child_cmd.arg("--spanner-database").arg(sp);
                }

                let pid_file = std::env::temp_dir().join(format!(
                    "graft_mount_{}.pid",
                    blake3::hash(canon_mountpoint.to_string_lossy().as_bytes()).to_hex()
                ));
                let log_path = std::env::temp_dir().join(format!(
                    "graft_mount_{}.log",
                    blake3::hash(canon_mountpoint.to_string_lossy().as_bytes()).to_hex()
                ));
                let log_file = std::fs::File::create(&log_path)?;
                child_cmd.stdout(log_file.try_clone()?);
                child_cmd.stderr(log_file);

                let child = child_cmd.spawn()?;
                let pid = child.id();
                std::fs::write(&pid_file, pid.to_string())?;

                // Poll /proc/mounts for up to 3 seconds
                let start = std::time::Instant::now();
                let mut mounted = false;
                let mount_str = canon_mountpoint.to_string_lossy();
                while start.elapsed() < std::time::Duration::from_secs(3) {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
                        if mounts.lines().any(|l| l.contains(mount_str.as_ref())) {
                            mounted = true;
                            break;
                        }
                    }
                }

                if mounted {
                    println!("Graft FUSE mount started in background:");
                    println!("  Mountpoint: {}", canon_mountpoint.display());
                    println!("  PID:        {}", pid);
                    println!("  Revision:   {}", desc);
                    println!("  Log:        {}", log_path.display());
                    println!("  To unmount: graft unmount {}", canon_mountpoint.display());
                } else {
                    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
                    anyhow::bail!(
                        "Mount process (PID {}) did not become active within 3s. Log: {}",
                        pid,
                        log_content
                    );
                }
                return Ok(());
            }

            // 4. Mount the FUSE filesystem
            #[cfg(feature = "fuse")]
            {
                let tree = graft::vfs::VfsTree::from_cas(&tree_digest, blob_store.as_ref())?;
                let fs = graft::vfs::GraftFilesystem::from_tree(tree, blob_store)
                    .with_commit_time(commit_time);

                let session =
                    graft::vfs::spawn_mount_fuse(fs, &canon_mountpoint, None).map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to mount FUSE at {}: {}",
                            canon_mountpoint.display(),
                            e
                        )
                    })?;

                if !internal_daemon {
                    println!("Graft FUSE filesystem mounted at: {}", canon_mountpoint.display());
                    println!("Mounted {}: tree {}", desc, tree_digest);
                    println!("Press Ctrl-C (SIGINT) to unmount.");
                }

                // Wait for termination signal
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(async {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut sigterm = signal(SignalKind::terminate()).ok();
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {},
                        _ = async {
                            if let Some(ref mut st) = sigterm {
                                st.recv().await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => {},
                    }
                });

                if !internal_daemon {
                    println!("\nUnmounting cleanly...");
                }
                drop(session);
                if !internal_daemon {
                    println!("Unmounted: {}", canon_mountpoint.display());
                }
            }

            #[cfg(not(feature = "fuse"))]
            {
                anyhow::bail!("This 'graft' binary was built without FUSE support. Rebuild with: cargo build --features fuse");
            }
        }
        Commands::Unmount { mountpoint, lazy, storage: _ } => {
            let canon_path = safe_canonicalize(&mountpoint);
            let pid_file = std::env::temp_dir().join(format!(
                "graft_mount_{}.pid",
                blake3::hash(canon_path.to_string_lossy().as_bytes()).to_hex()
            ));

            // If a background daemon PID was recorded, terminate it
            if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
                let _ = std::fs::remove_file(&pid_file);
            }

            // Execute fusermount3 -u
            let mut cmd = std::process::Command::new("fusermount3");
            if lazy {
                cmd.arg("-u").arg("-z").arg(&mountpoint);
            } else {
                cmd.arg("-u").arg(&mountpoint);
            }

            let success = match cmd.status() {
                Ok(s) => s.success(),
                Err(_) => {
                    let mut fb = std::process::Command::new("fusermount");
                    if lazy {
                        fb.arg("-u").arg("-z").arg(&mountpoint);
                    } else {
                        fb.arg("-u").arg(&mountpoint);
                    }
                    match fb.status() {
                        Ok(s) => s.success(),
                        Err(_) => {
                            let mut um = std::process::Command::new("umount");
                            if lazy {
                                um.arg("-l").arg(&mountpoint);
                            } else {
                                um.arg(&mountpoint);
                            }
                            um.status().map(|s| s.success()).unwrap_or(false)
                        }
                    }
                }
            };

            if success {
                println!("Successfully unmounted: {}", mountpoint.display());
            } else {
                let still_mounted = if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
                    mounts.lines().any(|l| l.contains(canon_path.to_string_lossy().as_ref()))
                } else {
                    false
                };
                if !still_mounted {
                    println!("Unmounted: {}", mountpoint.display());
                } else {
                    anyhow::bail!("Failed to unmount {}", mountpoint.display());
                }
            }
        }
        Commands::Gc {
            dry_run,
            grace_period_hours,
            grace_period_secs,
            prune_remote,
            allow_empty_roots,
            keep_roots,
            storage_dir,
            storage,
        } => {
            let store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;
            let ws_manager = WorkspaceManager::new(resolve_storage_dir(&storage_dir));

            // Parse any explicit keep-roots
            let mut extra_roots = Vec::new();
            for r in &keep_roots {
                let digest = if let Ok((_, d, _)) =
                    resolve_revision(r, store.as_ref(), meta_store.as_ref())
                {
                    d
                } else {
                    Digest::parse(r).map_err(|e| {
                        anyhow::anyhow!("Failed to parse --keep-root '{}': {}", r, e)
                    })?
                };
                extra_roots.push(digest);
            }

            println!("[GC] Starting Mark-and-Sweep Garbage Collection...");
            if dry_run {
                println!("[GC] Mode: DRY-RUN (no blobs will be deleted)");
            }

            let roots =
                graft::cas::collect_roots(meta_store.as_ref(), Some(&ws_manager), &extra_roots)?;
            println!(
                "[GC] Discovered {} live root references across metadata and workspaces.",
                roots.len()
            );

            let grace_period = if let Some(secs) = grace_period_secs {
                std::time::Duration::from_secs(secs)
            } else {
                std::time::Duration::from_secs(grace_period_hours * 3600)
            };

            let options = graft::cas::GcOptions {
                grace_period,
                dry_run,
                prune_remote,
                now: std::time::SystemTime::now(),
                allow_empty_roots,
            };

            let stats = graft::cas::run_gc(&roots, store.as_ref(), &options)?;

            println!("\n=== Garbage Collection Report ===");
            println!("Live roots discovered  : {}", stats.roots_count);
            println!(
                "Reachable live blobs   : {} ({:.2} MB)",
                stats.live_blobs,
                stats.live_bytes as f64 / 1_048_576.0
            );
            println!(
                "Protected by grace     : {} ({:.2} MB) [modified < {:?}]",
                stats.skipped_recent_blobs,
                stats.skipped_recent_bytes as f64 / 1_048_576.0,
                grace_period
            );
            if dry_run {
                println!(
                    "Reclaimable garbage    : {} ({:.2} MB) [DRY-RUN - not deleted]",
                    stats.garbage_blobs,
                    stats.garbage_bytes as f64 / 1_048_576.0
                );
            } else {
                println!(
                    "Garbage reclaimed      : {} ({:.2} MB)",
                    stats.garbage_blobs,
                    stats.garbage_bytes as f64 / 1_048_576.0
                );
            }
            println!("Total execution time   : {:.2?}", stats.elapsed);
            println!("=================================");
        }
        Commands::Spanner { action } => match action {
            SpannerCommands::Ddl { prefix } => {
                let dummy_path = "projects/p/instances/i/databases/d";
                let mut config = graft::metadata::spanner::SpannerConfig::new(dummy_path)?;
                if let Some(p) = prefix {
                    config.table_prefix = p;
                }
                for stmt in config.ddl_schema() {
                    println!("{};\n", stmt);
                }
            }
            SpannerCommands::Init { database, dry_run } => {
                let db_path = database
                    .or_else(|| std::env::var("GRAFT_SPANNER_DATABASE").ok())
                    .or_else(|| graft::config::GraftConfigFile::load().spanner_database)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Missing Spanner database path. Specify --database, set GRAFT_SPANNER_DATABASE, or run 'graft config set spanner_database <path>'."
                        )
                    })?;

                let config = graft::metadata::spanner::SpannerConfig::new(&db_path)?;
                let stmts = config.ddl_schema();

                if dry_run {
                    println!(
                        "Dry run: would execute {} DDL statements on '{}':\n",
                        stmts.len(),
                        db_path
                    );
                    for stmt in &stmts {
                        println!("{};\n", stmt);
                    }
                    return Ok(());
                }

                println!("Initializing Cloud Spanner schema on '{}'...", db_path);
                let store = graft::metadata::SpannerMetadataStore::connect_gcp(&db_path)?;
                store.init_schema()?;
                println!("✓ Successfully initialized Graft tables in Cloud Spanner:");
                println!("  - graft_refs");
                println!("  - graft_commits");
                println!("  - graft_commit_parents");
                println!("  - graft_workspace_leases");
            }
        },
        Commands::Config { action } => match action {
            ConfigCommands::Set { key, value } => {
                let mut cfg = graft::config::GraftConfigFile::load();
                cfg.set(&key, &value)?;
                println!("✓ Configuration updated: {} = {}", key, value);
                if let Some(p) = graft::config::GraftConfigFile::default_path() {
                    println!("  Saved to: {}", p.display());
                }
            }
            ConfigCommands::Get { key } => {
                let cfg = graft::config::GraftConfigFile::load();
                if let Some(val) = cfg.get(&key) {
                    println!("{}", val);
                } else {
                    eprintln!("Configuration key '{}' is not set.", key);
                    std::process::exit(1);
                }
            }
            ConfigCommands::Unset { key } => {
                let mut cfg = graft::config::GraftConfigFile::load();
                if cfg.unset(&key)? {
                    println!("✓ Removed configuration key: {}", key);
                } else {
                    println!("Notice: configuration key '{}' was not set.", key);
                }
            }
            ConfigCommands::List => {
                let cfg = graft::config::GraftConfigFile::load();
                let path_str = graft::config::GraftConfigFile::default_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                println!("Graft Configuration (file: {}):", path_str);
                let entries = cfg.list();
                if entries.is_empty() {
                    println!("  (No custom configuration set. Using system defaults.)");
                } else {
                    for (k, v) in entries {
                        println!("  {} = {}", k, v);
                    }
                }
                println!("\nActive Resolved Values:");
                let storage_args = StorageArgs {
                    cas_dir: None,
                    remote: None,
                    metadata: None,
                    spanner_database: None,
                    local: false,
                };
                println!("  cas_dir          = {}", storage_args.resolved_cas_dir().display());
                println!("  storage_dir      = {}", resolve_storage_dir(Path::new("/tmp/graft/storage")).display());
                if let Some(r) = storage_args.resolved_remote() {
                    println!("  remote           = {}", r);
                } else {
                    println!("  remote           = <none> (local NVMe CAS)");
                }
                if let Some(m) = storage_args.resolved_metadata() {
                    println!("  metadata         = {}", m);
                } else {
                    println!("  metadata         = <default> (local SQLite metadata.db)");
                }
                if let Some(s) = storage_args.resolved_spanner_database() {
                    println!("  spanner_database = {}", s);
                } else {
                    println!("  spanner_database = <none> (using local SQLite)");
                }
            }
            ConfigCommands::Path => {
                if let Some(p) = graft::config::GraftConfigFile::default_path() {
                    println!("{}", p.display());
                } else {
                    eprintln!("Could not determine configuration file path (HOME not set).");
                }
            }
        },
        Commands::Web {
            port,
            host,
            branch,
            storage_dir,
            open,
            storage,
        } => {
            let blob_store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;

            let config = graft::web::WebConfig {
                host: host.clone(),
                port,
                default_branch: branch,
                storage_dir: resolve_storage_dir(&storage_dir),
            };

            if open {
                let url = format!("http://localhost:{}", port);
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(graft::web::start_web_server(config, blob_store, meta_store))?;
        }
        Commands::Daemon { daemon, storage } => {
            let blob_store = storage.open_store()?;
            let meta_store = storage.open_metadata()?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(graft::daemon::run_server(daemon, blob_store, meta_store))?;
        }
    }

    Ok(())
}
