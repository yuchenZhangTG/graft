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

//! # `graftd` Server-Centric Monorepo Daemon Subsystem
//!
//! Provides the central coordination service for distributed fleets of autonomous
//! AI agents:
//! - Centralized Content-Addressed Storage (CAS) with FastCDC deduplication.
//! - Atomic Compare-And-Swap (CAS) reference and branch coordination.
//! - Server-Side 3-Way AST Semantic Merges (0-download merges in < 20ms).
//! - Distributed agent workspace lease management.

pub mod cas_service;
pub mod client;
pub mod lease_service;
pub mod merge_service;
pub mod ref_service;
pub mod server;

pub use client::{RemoteBlobStore, RemoteMetadataStore};
pub use merge_service::{ServerSideMergeRequest, ServerSideMergeResponse};
pub use server::{serve_with_listener, DaemonState};

use crate::cas::blob_store::BlobStore;
use crate::metadata::MetadataStore;
use std::io;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Command-line arguments for launching `graftd`.
#[derive(clap::Args, Clone, Debug)]
pub struct DaemonArgs {
    #[arg(long, default_value = "9090", help = "Port to listen on")]
    pub port: u16,

    #[arg(long, default_value = "0.0.0.0", help = "Host / IP to bind to")]
    pub host: String,

    #[arg(long, env = "GRAFT_AUTH_TOKEN", help = "Optional bearer authentication token")]
    pub auth_token: Option<String>,
}

/// Starts the `graftd` server using the provided storage and metadata backends.
pub async fn run_server(
    args: DaemonArgs,
    blob_store: Arc<dyn BlobStore>,
    meta_store: Arc<dyn MetadataStore>,
) -> io::Result<()> {
    let bind_addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&bind_addr).await?;

    let state = Arc::new(DaemonState {
        blob_store,
        meta_store,
        auth_token: args.auth_token,
    });

    println!("╔════════════════════════════════════════════════════════════════════╗");
    println!("║                  ⚡ GRAFTD MONOREPO DAEMON                         ║");
    println!("╠════════════════════════════════════════════════════════════════════╣");
    println!("║  Listening:    http://{}                                  ║", bind_addr);
    println!("║  Protocol:     HTTP/1.1 REST + Raw Binary Streams (Tokio)          ║");
    println!("║  Services:     CAS, Refs, AST Semantic Merge, Workspace Leases     ║");
    println!("╚════════════════════════════════════════════════════════════════════╝");
    println!("Ready for autonomous AI agent fleet coordination. Press Ctrl-C to stop.");

    serve_with_listener(listener, state, None).await
}
