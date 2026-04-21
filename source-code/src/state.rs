use miette::{Result, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use crate::STORE_PATH;

const STATE_PATH: &str = "/var/lib/hpm/state.json";

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VersionInfo {
    pub checksum: String,
    pub pinned: bool,
    /// true  = user explicitly asked for this package
    /// false = installed as a dependency of another package
    #[serde(default = "default_true")]
    pub manually_installed: bool,
    /// Which packages directly depend on this package@version.
    /// Updated on install and remove.
    #[serde(default)]
    pub required_by: HashSet<String>,  // "pkgname@version"
    /// Packages this version directly depends on ("pkgname@version").
    #[serde(default)]
    pub depends_on: HashSet<String>,
    /// Packages this package conflicts with (cannot be installed together).
    #[serde(default)]
    pub conflicts_with: HashSet<String>,
    /// Timestamp of installation (Unix seconds).
    #[serde(default)]
    pub installed_at: u64,
}

fn default_true() -> bool { true }

impl VersionInfo {
    pub fn new(checksum: &str, manually_installed: bool) -> Self {
        Self {
            checksum: checksum.to_string(),
            pinned: false,
            manually_installed,
            required_by: HashSet::new(),
            depends_on: HashSet::new(),
            conflicts_with: HashSet::new(),
            installed_at: unix_now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Rollback history
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RollbackEntry {
    /// Unix timestamp when this snapshot was created.
    pub timestamp: u64,
    /// Human-readable description: "install foo@1.0.0", "remove bar@2.0.0"
    pub description: String,
    /// Snapshot of packages at this point in time.
    /// key = pkgname, value = map of version → VersionInfo
    pub snapshot: HashMap<String, HashMap<String, VersionInfo>>,
}

// ---------------------------------------------------------------------------
// Main State struct
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct State {
    /// key = package name, value = map of version → VersionInfo
    pub packages: HashMap<String, HashMap<String, VersionInfo>>,

    /// Rollback history (newest last, capped at MAX_HISTORY entries).
    #[serde(default)]
    pub history: Vec<RollbackEntry>,
}

const MAX_HISTORY: usize = 20;

impl State {
    // ── Load / Save ─────────────────────────────────────────────────────────

    pub fn load() -> Result<Self> {
        if !Path::new(STATE_PATH).exists() {
            return Ok(State::default());
        }
        let data = fs::read(STATE_PATH).into_diagnostic()?;
        Ok(serde_json::from_slice(&data).into_diagnostic()?)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = Path::new(STATE_PATH).parent() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
        let data = serde_json::to_vec_pretty(self).into_diagnostic()?;
        let tmp = format!("{}.tmp", STATE_PATH);
        fs::write(&tmp, &data).into_diagnostic()?;
        fs::rename(&tmp, STATE_PATH).into_diagnostic()?;
        Ok(())
    }

    // ── Snapshot / rollback ──────────────────────────────────────────────────

    /// Save current state as a rollback snapshot before a mutation.
    pub fn push_snapshot(&mut self, description: &str) {
        let entry = RollbackEntry {
            timestamp: unix_now(),
            description: description.to_string(),
            snapshot: self.packages.clone(),
        };
        self.history.push(entry);
        // Cap history size
        if self.history.len() > MAX_HISTORY {
            let drain = self.history.len() - MAX_HISTORY;
            self.history.drain(0..drain);
        }
    }

    /// Restore the most recent snapshot (pop it from history).
    pub fn pop_snapshot(&mut self) -> Option<RollbackEntry> {
        self.history.pop()
    }

    /// List history entries (index, timestamp, description).
    pub fn list_history(&self) -> Vec<(usize, u64, &str)> {
        self.history.iter().enumerate()
        .map(|(i, e)| (i, e.timestamp, e.description.as_str()))
        .collect()
    }

    /// Restore packages to a specific history index.
    pub fn restore_snapshot(&mut self, index: usize) -> bool {
        if index >= self.history.len() { return false; }
        let snapshot = self.history[index].snapshot.clone();
        // Save current state as a new entry before overwriting
        self.push_snapshot("pre-rollback snapshot");
        self.packages = snapshot;
        true
    }

    // ── Package mutation ─────────────────────────────────────────────────────

    /// Register a newly installed package.
    pub fn update_package(
        &mut self,
        package: &str,
        version: &str,
        checksum: &str,
        manually_installed: bool,
        depends_on: HashSet<String>,
        conflicts_with: HashSet<String>,
    ) {
        let mut info = VersionInfo::new(checksum, manually_installed);
        info.depends_on = depends_on.clone();
        info.conflicts_with = conflicts_with;

        self.packages
        .entry(package.to_string())
        .or_default()
        .insert(version.to_string(), info);

        // Update required_by for all direct deps
        let pkg_ver_key = format!("{}@{}", package, version);
        for dep_spec in &depends_on {
            // dep_spec is "pkgname@version" or just "pkgname"
            let (dep_name, dep_ver) = split_pkg_ver(dep_spec);
            if let Some(vers) = self.packages.get_mut(&dep_name) {
                // Update the specific version if known, else all installed versions
                let targets: Vec<String> = if dep_ver.is_empty() {
                    vers.keys().cloned().collect()
                } else {
                    vec![dep_ver.to_string()]
                };
                for t in targets {
                    if let Some(vi) = vers.get_mut(&t) {
                        vi.required_by.insert(pkg_ver_key.clone());
                    }
                }
            }
        }
    }

    /// Remove a package version and clean up reverse dependency records.
    pub fn remove_package_version(&mut self, package: &str, version: &str) {
        let pkg_ver_key = format!("{}@{}", package, version);

        // Collect deps this version had so we can clean required_by
        let deps: HashSet<String> = self.packages.get(package)
        .and_then(|vs| vs.get(version))
        .map(|vi| vi.depends_on.clone())
        .unwrap_or_default();

        // Remove required_by entries from dependencies
        for dep_spec in &deps {
            let (dep_name, dep_ver) = split_pkg_ver(dep_spec);
            if let Some(vers) = self.packages.get_mut(&dep_name) {
                let targets: Vec<String> = if dep_ver.is_empty() {
                    vers.keys().cloned().collect()
                } else {
                    vec![dep_ver.to_string()]
                };
                for t in targets {
                    if let Some(vi) = vers.get_mut(&t) {
                        vi.required_by.remove(&pkg_ver_key);
                    }
                }
            }
        }

        // Remove the version itself
        if let Some(vers) = self.packages.get_mut(package) {
            vers.remove(version);
            if vers.is_empty() {
                self.packages.remove(package);
            }
        }
    }

    // ── Queries ──────────────────────────────────────────────────────────────

    /// Return all packages that currently depend on `package` (any version).
    pub fn reverse_deps(&self, package: &str) -> Vec<String> {
        let mut result = Vec::new();
        for (name, vers) in &self.packages {
            if name == package { continue; }
            for (ver, info) in vers {
                for dep in &info.depends_on {
                    let (dep_name, _) = split_pkg_ver(dep);
                    if dep_name == package {
                        result.push(format!("{}@{}", name, ver));
                        break;
                    }
                }
            }
        }
        result.sort();
        result.dedup();
        result
    }

    /// Return packages that are auto-installed and have no remaining required_by.
    pub fn orphans(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for (name, vers) in &self.packages {
            for (ver, info) in vers {
                if !info.manually_installed && info.required_by.is_empty() {
                    result.push((name.clone(), ver.clone()));
                }
            }
        }
        result.sort();
        result
    }

    /// Check if installing `package` would violate any conflict declarations.
    /// Returns list of conflict descriptions if any.
    pub fn check_conflicts(&self, package: &str, declared_conflicts: &[String]) -> Vec<String> {
        let mut violations = Vec::new();

        // Check if anything already installed conflicts with the new package
        for (installed_name, vers) in &self.packages {
            for (installed_ver, info) in vers {
                // Does installed package conflict with new package?
                for conf in &info.conflicts_with {
                    let (conf_name, _) = split_pkg_ver(conf);
                    if conf_name == package {
                        violations.push(format!(
                            "{}@{} conflicts with {}",
                            installed_name, installed_ver, package
                        ));
                    }
                }
            }
        }

        // Does new package conflict with anything installed?
        for conf in declared_conflicts {
            let (conf_name, _) = split_pkg_ver(conf);
            if self.packages.contains_key(&conf_name) {
                violations.push(format!(
                    "{} conflicts with installed package {}",
                    package, conf_name
                ));
            }
        }

        violations
    }

    /// Current version symlink target.
    pub fn get_current_version(&self, package: &str) -> Option<String> {
        let current_link = format!("{}{}/current", STORE_PATH, package);
        if let Ok(target) = fs::read_link(&current_link) {
            if let Some(ver) = target.file_name()?.to_str() {
                return Some(ver.to_string());
            }
        }
        None
    }

    /// Previous version for a package (second-newest installed).
    pub fn get_previous_version(&self, package: &str) -> Option<String> {
        let current = self.get_current_version(package)?;
        let vers = self.packages.get(package)?;
        let mut all: Vec<&String> = vers.keys().collect();
        all.sort_by(|a, b| crate::utils::compare_versions(a, b));
        // Return the version just before current
        let pos = all.iter().position(|v| *v == &current)?;
        if pos == 0 { return None; }
        Some(all[pos - 1].to_string())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// Split "pkgname@version" → ("pkgname", "version").
/// Returns ("pkgname", "") if no @ present.
pub fn split_pkg_ver(spec: &str) -> (String, String) {
    if let Some(at) = spec.find('@') {
        (spec[..at].to_string(), spec[at + 1..].to_string())
    } else {
        (spec.to_string(), String::new())
    }
}
