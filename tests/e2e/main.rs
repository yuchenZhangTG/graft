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

//! # Graft End-to-End (E2E) Test Suite
//!
//! Orchestrates end-to-end integration tests in isolated sandbox environments.

mod agent_workflow;
mod branch_merge_workflow;
mod cherry_pick_workflow;
mod chunking_workflow;
mod daemon_workflow;
mod fuse_mount_workflow;
mod gc_workflow;
mod gcs_workflow;
mod local_first_workflow;
mod merge_workflow;
mod spanner_workflow;
mod workspace_mount_workflow;
