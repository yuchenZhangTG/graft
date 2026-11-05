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

//! # `graftd`: Standalone Monorepo Daemon Binary
//!
//! Central coordination service for fleets of autonomous AI coding agents.

use clap::Parser;
use graft::config::StorageArgs;
use graft::daemon::{run_server, DaemonArgs};

#[derive(Parser, Debug)]
#[command(
    name = "graftd",
    author = "Graft Contributors <agent@graft.dev>",
    version = "0.2.0",
    about = "graftd: Server-centric monorepo daemon for autonomous AI agent fleets"
)]
struct GraftdCli {
    #[command(flatten)]
    pub daemon: DaemonArgs,

    #[command(flatten)]
    pub storage: StorageArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = GraftdCli::parse();
    let blob_store = cli.storage.open_store()?;
    let meta_store = cli.storage.open_metadata()?;

    run_server(cli.daemon, blob_store, meta_store).await?;
    Ok(())
}
