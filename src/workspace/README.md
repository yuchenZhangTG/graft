# Workspace Management (`src/workspace/`)

Coordinates isolated, ephemeral Copy-on-Write workspaces for concurrent AI coding agents.

## Key Capabilities

- **Instant $O(1)$ Workspace Initialization**: Creates independent agent workspaces without copying files.
- **Sub-Millisecond $O(K)$ Delta Scanning**: Inspects upperdir directly for modified, created, or deleted files without scanning the lower repository tree, supporting both kernel char-device `(0, 0)` and user-space `.wh.<name>` whiteout tombstones.
- **Interactive Writeable Workspace Mounting**: Unprivileged FUSE CoW (`graft workspace mount`) and kernel OverlayFS (`--backend overlay`), background daemon supervision, `/proc/mounts` tracking, and graceful unmounting (`graft workspace unmount`).
- **Branch Tracking & Ancestry Chaining**: Tracks the active branch and base commit in `workspace.json`, automatically chaining sequential commits.
- **Distributed Lease Fencing**: Auto-acquires workspace leases on creation, verifies ownership during `graft commit` to prevent split-brain conflicts, and releases on destroy.

## Files

- [`workspace.rs`](../workspace.rs): Workspace coordinator (`WorkspaceManager`, `WorkspaceConfig`) managing writeable mount lifecycles (FUSE CoW and kernel OverlayFS), scanning upperdir change sets, process PID tracking, and persisting metadata (`workspace.json`).
