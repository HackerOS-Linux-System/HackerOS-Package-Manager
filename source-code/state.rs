use miette::{Result, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

// Uwaga: ścieżki do starych plików JSON (state.json, wrapper-names.json) nie
// są już tu potrzebne — od 0.9 both State i WrapperNames są trzymane w
// ~/.hackeros/hpm/db/hpm.db (SQLite, patrz `db.rs`). Jednorazowa migracja ze
// starych plików JSON żyje w `db::migrate_legacy_json`.

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VersionInfo {
    pub checksum: String,
    pub pinned: bool,
    #[serde(default = "default_true")]
    pub manually_installed: bool,
    #[serde(default)]
    pub required_by: HashSet<String>,
    #[serde(default)]
    pub depends_on: HashSet<String>,
    #[serde(default)]
    pub conflicts_with: HashSet<String>,
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
    pub timestamp: u64,
    pub description: String,
    pub snapshot: HashMap<String, HashMap<String, VersionInfo>>,
}

// ---------------------------------------------------------------------------
// Wrapper name persistence
// ---------------------------------------------------------------------------

/// Mapa: "pkg_name:bin_name" → "wrapper_name_in_usr_bin"
/// Przechowuje wybory użytkownika przy konfliktach nazw wrapperów.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct WrapperNames {
    /// key: "pkgname:binname", value: rzeczywista nazwa pliku w /usr/bin
    pub names: HashMap<String, String>,
}

impl WrapperNames {
    pub fn load() -> Self {
        crate::db::load_wrapper_names().unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = crate::db::save_wrapper_names(self);
    }

    /// Klucz do mapy.
    pub fn key(pkg_name: &str, bin_name: &str) -> String {
        format!("{}:{}", pkg_name, bin_name)
    }

    /// Pobierz zapamiętaną nazwę wrappera. None = nie ma zapamiętanej.
    pub fn get(&self, pkg_name: &str, bin_name: &str) -> Option<&str> {
        self.names.get(&Self::key(pkg_name, bin_name)).map(|s| s.as_str())
    }

    /// Zapisz wybór użytkownika.
    pub fn set(&mut self, pkg_name: &str, bin_name: &str, wrapper_name: &str) {
        self.names.insert(Self::key(pkg_name, bin_name), wrapper_name.to_string());
        self.save();
    }

    /// Usuń wpis (np. po odinstalowaniu pakietu).
    pub fn remove_pkg(&mut self, pkg_name: &str) {
        let prefix = format!("{}:", pkg_name);
        self.names.retain(|k, _| !k.starts_with(&prefix));
        self.save();
    }
}

// ---------------------------------------------------------------------------
// Main State struct
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct State {
    pub packages: HashMap<String, HashMap<String, VersionInfo>>,
    #[serde(default)]
    pub history: Vec<RollbackEntry>,
}

const MAX_HISTORY: usize = 20;

impl State {
    // ── Load / Save ─────────────────────────────────────────────────────────

    pub fn load() -> Result<Self> {
        crate::db::load_state()
    }

    pub fn save(&self) -> Result<()> {
        crate::db::save_state(self)
    }

    // ── Snapshot / rollback ──────────────────────────────────────────────────

    pub fn push_snapshot(&mut self, description: &str) {
        let entry = RollbackEntry {
            timestamp:   unix_now(),
            description: description.to_string(),
            snapshot:    self.packages.clone(),
        };
        self.history.push(entry);
        if self.history.len() > MAX_HISTORY {
            let drain = self.history.len() - MAX_HISTORY;
            self.history.drain(0..drain);
        }
    }

    pub fn pop_snapshot(&mut self) -> Option<RollbackEntry> {
        self.history.pop()
    }

    pub fn list_history(&self) -> Vec<(usize, u64, &str)> {
        self.history.iter().enumerate()
            .map(|(i, e)| (i, e.timestamp, e.description.as_str()))
            .collect()
    }

    pub fn restore_snapshot(&mut self, index: usize) -> bool {
        if index >= self.history.len() { return false; }
        let snapshot = self.history[index].snapshot.clone();
        self.push_snapshot("pre-rollback snapshot");
        self.packages = snapshot;
        true
    }

    // ── Package mutation ─────────────────────────────────────────────────────

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
        info.depends_on    = depends_on.clone();
        info.conflicts_with = conflicts_with;

        self.packages
            .entry(package.to_string())
            .or_default()
            .insert(version.to_string(), info);

        let pkg_ver_key = format!("{}@{}", package, version);
        for dep_spec in &depends_on {
            let (dep_name, dep_ver) = split_pkg_ver(dep_spec);
            if let Some(vers) = self.packages.get_mut(&dep_name) {
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

    pub fn remove_package_version(&mut self, package: &str, version: &str) {
        let pkg_ver_key = format!("{}@{}", package, version);

        let deps: HashSet<String> = self.packages.get(package)
            .and_then(|vs| vs.get(version))
            .map(|vi| vi.depends_on.clone())
            .unwrap_or_default();

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

        if let Some(vers) = self.packages.get_mut(package) {
            vers.remove(version);
            if vers.is_empty() {
                self.packages.remove(package);
            }
        }

        // Wyczyść persystentne nazwy wrapperów dla tego pakietu
        let mut wn = WrapperNames::load();
        wn.remove_pkg(package);
    }

    // ── Queries ──────────────────────────────────────────────────────────────

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

    pub fn check_conflicts(&self, package: &str, declared_conflicts: &[String]) -> Vec<String> {
        let mut violations = Vec::new();

        for (installed_name, vers) in &self.packages {
            for (installed_ver, info) in vers {
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

    pub fn get_current_version(&self, package: &str) -> Option<String> {
        let current_link = format!("{}{}/current", crate::store_path(), package);
        if let Ok(target) = fs::read_link(&current_link) {
            if let Some(ver) = target.file_name()?.to_str() {
                return Some(ver.to_string());
            }
        }
        None
    }

    pub fn get_previous_version(&self, package: &str) -> Option<String> {
        let current = self.get_current_version(package)?;
        let vers    = self.packages.get(package)?;
        let mut all: Vec<&String> = vers.keys().collect();
        all.sort_by(|a, b| crate::utils::compare_versions(a, b));
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

pub fn split_pkg_ver(spec: &str) -> (String, String) {
    if let Some(at) = spec.find('@') {
        (spec[..at].to_string(), spec[at + 1..].to_string())
    } else {
        (spec.to_string(), String::new())
    }
}
