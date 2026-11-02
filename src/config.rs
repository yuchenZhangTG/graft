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

//! # User & System Configuration ($HOME/.graft/config.json)
//!
//! Provides persistent configuration management for Graft, removing the need
//! to export environment variables on every terminal session.
//!
//! ### Configuration Resolution Precedence:
//! 1. **Command-Line Flags** (e.g. `--remote gs://...`, `--spanner-database ...`)
//! 2. **Environment Variables** (e.g. `GRAFT_REMOTE`, `GRAFT_SPANNER_DATABASE`)
//! 3. **User Configuration File** (`$HOME/.graft/config.json` or `$HOME/.graft/config`)
//! 4. **Built-in Defaults** (`/tmp/graft/cas`, `/tmp/graft/storage`)

use std::fs;
use std::io;
use std::path::PathBuf;

/// Persistent configuration stored in `$HOME/.graft/config.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GraftConfigFile {
    /// Default remote CAS URL (e.g. `gs://<bucket>/<prefix>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,

    /// Default Cloud Spanner database resource path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spanner_database: Option<String>,

    /// Default local CAS directory (L1 cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cas_dir: Option<PathBuf>,

    /// Default storage directory for workspace CoW layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_dir: Option<PathBuf>,

    /// Default agent identifier string (defaults to `$USER@$HOSTNAME`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

impl GraftConfigFile {
    /// Discovers the path to the configuration file.
    /// Checks `GRAFT_CONFIG_PATH`, then `$HOME/.graft/config.json`, then `$HOME/.graft/config`.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("GRAFT_CONFIG_PATH") {
            return Some(PathBuf::from(p));
        }

        if let Ok(home) = std::env::var("HOME") {
            let graft_dir = PathBuf::from(home).join(".graft");
            let json_path = graft_dir.join("config.json");
            if json_path.exists() {
                return Some(json_path);
            }
            let text_path = graft_dir.join("config");
            if text_path.exists() {
                return Some(text_path);
            }
            // Default target for new writes
            return Some(json_path);
        }

        None
    }

    /// Loads configuration from disk using default discovery path.
    /// Returns default empty configuration if the file does not exist.
    pub fn load() -> Self {
        Self::load_from_path(Self::default_path())
    }

    /// Loads configuration from an explicit file path.
    pub fn load_from_path(path: Option<PathBuf>) -> Self {
        let path = match path {
            Some(p) if p.exists() => p,
            _ => return Self::default(),
        };

        if let Ok(content) = fs::read_to_string(&path) {
            // 1. Try parsing as JSON
            if let Ok(cfg) = serde_json::from_str::<GraftConfigFile>(&content) {
                return cfg;
            }

            // 2. Fall back to parsing key = value INI-style syntax
            let mut cfg = GraftConfigFile::default();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
                    continue;
                }
                if let Some((k, v)) = trimmed.split_once('=') {
                    let key = k.trim().to_lowercase();
                    let val = v.trim().trim_matches('"').trim_matches('\'');
                    match key.as_str() {
                        "remote" => cfg.remote = Some(val.to_string()),
                        "spanner_database" | "spanner" | "database" => {
                            cfg.spanner_database = Some(val.to_string())
                        }
                        "cas_dir" | "cas" => cfg.cas_dir = Some(PathBuf::from(val)),
                        "storage_dir" | "storage" => cfg.storage_dir = Some(PathBuf::from(val)),
                        "agent_id" | "agent" => cfg.agent_id = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
            return cfg;
        }

        Self::default()
    }

    /// Saves configuration to `$HOME/.graft/config.json`.
    pub fn save(&self) -> io::Result<PathBuf> {
        let path = Self::default_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Could not determine HOME directory for ~/.graft/config.json",
            )
        })?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json_str = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        fs::write(&path, json_str)?;
        Ok(path)
    }

    /// Retrieves a configuration field by string key.
    pub fn get(&self, key: &str) -> Option<String> {
        match key.to_lowercase().as_str() {
            "remote" => self.remote.clone(),
            "spanner_database" | "spanner" => self.spanner_database.clone(),
            "cas_dir" | "cas" => self.cas_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
            "storage_dir" | "storage" => {
                self.storage_dir.as_ref().map(|p| p.to_string_lossy().to_string())
            }
            "agent_id" | "agent" => self.agent_id.clone(),
            _ => None,
        }
    }

    /// Sets a configuration field by string key.
    pub fn set(&mut self, key: &str, value: &str) -> io::Result<()> {
        match key.to_lowercase().as_str() {
            "remote" => self.remote = Some(value.to_string()),
            "spanner_database" | "spanner" => self.spanner_database = Some(value.to_string()),
            "cas_dir" | "cas" => self.cas_dir = Some(PathBuf::from(value)),
            "storage_dir" | "storage" => self.storage_dir = Some(PathBuf::from(value)),
            "agent_id" | "agent" => self.agent_id = Some(value.to_string()),
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Unknown config key '{}'. Valid keys: remote, spanner_database, cas_dir, storage_dir, agent_id",
                        unknown
                    ),
                ));
            }
        }
        self.save()?;
        Ok(())
    }

    /// Removes a configuration field by string key.
    pub fn unset(&mut self, key: &str) -> io::Result<bool> {
        let removed = match key.to_lowercase().as_str() {
            "remote" => self.remote.take().is_some(),
            "spanner_database" | "spanner" => self.spanner_database.take().is_some(),
            "cas_dir" | "cas" => self.cas_dir.take().is_some(),
            "storage_dir" | "storage" => self.storage_dir.take().is_some(),
            "agent_id" | "agent" => self.agent_id.take().is_some(),
            _ => false,
        };
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Lists all non-empty key-value pairs.
    pub fn list(&self) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        if let Some(ref r) = self.remote {
            entries.push(("remote".to_string(), r.clone()));
        }
        if let Some(ref s) = self.spanner_database {
            entries.push(("spanner_database".to_string(), s.clone()));
        }
        if let Some(ref c) = self.cas_dir {
            entries.push(("cas_dir".to_string(), c.to_string_lossy().to_string()));
        }
        if let Some(ref s) = self.storage_dir {
            entries.push(("storage_dir".to_string(), s.to_string_lossy().to_string()));
        }
        if let Some(ref a) = self.agent_id {
            entries.push(("agent_id".to_string(), a.clone()));
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_config_json_roundtrip() -> io::Result<()> {
        let tmp = tempdir()?;
        let cfg_path = tmp.path().join("config.json");

        let mut cfg = GraftConfigFile::default();
        cfg.remote = Some("gs://my-bucket/repo".to_string());
        cfg.spanner_database = Some("projects/p/instances/i/databases/d".to_string());
        cfg.cas_dir = Some(PathBuf::from("/mnt/nvme/cas"));
        cfg.storage_dir = Some(PathBuf::from("/mnt/nvme/storage"));
        cfg.agent_id = Some("agent-007".to_string());

        let json_str = serde_json::to_string_pretty(&cfg).unwrap();
        fs::write(&cfg_path, json_str)?;

        let loaded = GraftConfigFile::load_from_path(Some(cfg_path.clone()));
        assert_eq!(loaded, cfg);

        // Test getters
        assert_eq!(loaded.get("remote").unwrap(), "gs://my-bucket/repo");
        assert_eq!(
            loaded.get("spanner_database").unwrap(),
            "projects/p/instances/i/databases/d"
        );
        assert_eq!(loaded.get("cas_dir").unwrap(), "/mnt/nvme/cas");

        Ok(())
    }

    #[test]
    fn test_config_ini_style_fallback() -> io::Result<()> {
        let tmp = tempdir()?;
        let cfg_path = tmp.path().join("config");

        let ini_content = r#"
# Graft configuration
remote = "gs://fallback-bucket/repo"
spanner_database = projects/test/instances/dev/databases/meta
cas_dir = /tmp/custom_cas
"#;
        fs::write(&cfg_path, ini_content)?;

        let loaded = GraftConfigFile::load_from_path(Some(cfg_path));
        assert_eq!(loaded.remote.unwrap(), "gs://fallback-bucket/repo");
        assert_eq!(
            loaded.spanner_database.unwrap(),
            "projects/test/instances/dev/databases/meta"
        );
        assert_eq!(loaded.cas_dir.unwrap(), PathBuf::from("/tmp/custom_cas"));

        Ok(())
    }
}
