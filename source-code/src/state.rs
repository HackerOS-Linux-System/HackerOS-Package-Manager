use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use crate::STORE_PATH;

const STATE_PATH: &str = "/var/lib/hpm/state.json";

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct State {
    pub packages: HashMap<String, HashMap<String, VersionInfo>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VersionInfo {
    pub checksum: String,
    pub pinned: bool,
}

impl State {
    pub fn load() -> Result<Self> {
        if !Path::new(STATE_PATH).exists() {
            return Ok(State::default());
        }
        let data = fs::read(STATE_PATH)?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_vec(self)?;
        let tmp_path = format!("{}.tmp", STATE_PATH);
        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, STATE_PATH)?;
        Ok(())
    }

    pub fn update_package(&mut self, package: &str, version: &str, checksum: &str) {
        self.packages
        .entry(package.to_string())
        .or_insert_with(HashMap::new)
        .insert(version.to_string(), VersionInfo {
            checksum: checksum.to_string(),
                pinned: false,
        });
    }

    pub fn remove_package_version(&mut self, package: &str, version: &str) {
        if let Some(vers) = self.packages.get_mut(package) {
            vers.remove(version);
            if vers.is_empty() {
                self.packages.remove(package);
            }
        }
    }

    pub fn get_current_version(&self, package: &str) -> Option<String> {
        let current_link = format!("{}{}/current", STORE_PATH, package);
        if let Ok(target) = fs::read_link(&current_link) {
            if let Some(ver) = target.file_name()?.to_str() {
                return Some(ver.to_string());
            }
        }
        None
    }
}
