use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::state::State;

fn system_lock_path() -> String { format!("{}/system.lock", crate::db_dir()) }

// ---------------------------------------------------------------------------
// Lock file structure
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    /// Wersja formatu lock file (dla kompatybilności wstecznej)
    pub lock_version: u32,
    /// Kiedy lock był ostatnio wygenerowany (Unix timestamp)
    pub generated_at: u64,
    /// Wersja hpm która wygenerowała lock
    pub hpm_version: String,
    /// Zainstalowane pakiety
    pub packages: HashMap<String, LockEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// Zainstalowana wersja
    pub version: String,
    /// Commit git z którego pochodzi pakiet
    pub git_commit: String,
    /// URL repozytorium git
    pub repo_url: String,
    /// SHA-256 hash zawartości (z state.json)
    pub checksum: String,
    /// Bezpośrednie zależności tego pakietu (name → version)
    pub dependencies: HashMap<String, String>,
    /// Czy pakiet był zainstalowany ręcznie czy jako zależność
    pub manually_installed: bool,
    /// Unix timestamp instalacji
    pub installed_at: u64,
}

impl LockFile {
    pub fn new() -> Self {
        Self {
            lock_version: 1,
            generated_at: unix_now(),
            hpm_version:  env!("CARGO_PKG_VERSION").to_string(),
            packages:     HashMap::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!(
                "Lock file not found at {}.\nRun {} to generate one.",
                path.display(), "hpm lock generate".bright_black()
            );
        }
        let data = fs::read(path).into_diagnostic()?;
        serde_json::from_slice(&data)
            .map_err(|e| miette::miette!("Invalid lock file: {}", e))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
        let data = serde_json::to_vec_pretty(self).into_diagnostic()?;
        let tmp  = path.with_extension("lock.tmp");
        fs::write(&tmp, &data).into_diagnostic()?;
        fs::rename(&tmp, path).into_diagnostic()?;
        Ok(())
    }

    /// Sprawdź czy bieżący stan instalacji pasuje do lock file.
    /// Zwraca listę rozbieżności.
    pub fn check_against_state(&self, state: &State) -> Vec<String> {
        let mut diffs = Vec::new();

        // Pakiety w lock ale nie zainstalowane
        for (name, entry) in &self.packages {
            match state.get_current_version(name) {
                None => {
                    diffs.push(format!("missing: {}@{} not installed", name, entry.version));
                }
                Some(installed_ver) => {
                    if installed_ver != entry.version {
                        diffs.push(format!(
                            "version mismatch: {} — lock has {}, installed has {}",
                            name, entry.version, installed_ver
                        ));
                    }
                    // Sprawdź checksum
                    if let Some(vers) = state.packages.get(name) {
                        if let Some(info) = vers.get(&installed_ver) {
                            if info.checksum != entry.checksum {
                                diffs.push(format!(
                                    "checksum mismatch: {}@{} — contents may have changed",
                                    name, installed_ver
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Pakiety zainstalowane ale nie w lock
        for (name, _) in &state.packages {
            if !self.packages.contains_key(name) {
                if let Some(ver) = state.get_current_version(name) {
                    diffs.push(format!("unlocked: {}@{} installed but not in lock file", name, ver));
                }
            }
        }

        diffs
    }
}

// ---------------------------------------------------------------------------
// Resolve lock file path
// ---------------------------------------------------------------------------

fn resolve_lock_path(project_path: Option<&str>) -> PathBuf {
    if let Some(p) = project_path {
        Path::new(p).join("hpm.lock")
    } else {
        // Jeśli jesteśmy w katalogu z info.hk — użyj lokalnego lock
        let cwd_lock = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("hpm.lock");
        if cwd_lock.parent().map(|p| p.join("info.hk").exists()).unwrap_or(false) {
            cwd_lock
        } else {
            PathBuf::from(system_lock_path())
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn lock(args: Vec<String>) -> Result<()> {
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("status");
    let project = args.get(1).map(|s| s.as_str());

    match subcmd {
        "generate" | "gen" | "update" => cmd_generate(project),
        "check"    | "verify"         => cmd_check(project),
        "status"                      => cmd_status(project),
        "diff"                        => cmd_diff(project),
        "--help" | "help"             => { print_help(); Ok(()) }
        _ => {
            eprintln!("{} Unknown lock subcommand: {}", "✗".red(), subcmd);
            print_help();
            std::process::exit(1);
        }
    }
}

/// Wygeneruj/zaktualizuj plik lock na podstawie bieżącego stanu.
fn cmd_generate(project: Option<&str>) -> Result<()> {
    let lock_path = resolve_lock_path(project);
    let state     = State::load()?;

    if state.packages.is_empty() {
        println!("{} No packages installed — lock file will be empty.", "→".bright_black());
    }

    let repo_mgr = crate::repo::RepoManager::load_sync()?;
    let mut lock = LockFile::new();

    println!("{} Generating lock file...", "→".white());

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        let info = match versions.get(&current_ver) {
            Some(i) => i,
            None    => continue,
        };

        // Pobierz commit hash z lokalnego repo (jeśli sklonowane)
        let git_commit = get_git_commit(pkg_name, &current_ver);
        let repo_url   = repo_mgr.get_package_url(pkg_name)
            .unwrap_or("unknown")
            .to_string();

        // Zależności bezpośrednie
        let mut deps = HashMap::new();
        for dep_spec in &info.depends_on {
            let (dep_name, dep_ver) = crate::state::split_pkg_ver(dep_spec);
            if !dep_ver.is_empty() {
                deps.insert(dep_name, dep_ver);
            } else if let Some(v) = state.get_current_version(&dep_name) {
                deps.insert(dep_name, v);
            }
        }

        lock.packages.insert(pkg_name.clone(), LockEntry {
            version:              current_ver.clone(),
            git_commit,
            repo_url,
            checksum:             info.checksum.clone(),
            dependencies:         deps,
            manually_installed:   info.manually_installed,
            installed_at:         info.installed_at,
        });

        println!("  {} {}@{}", "✔".red(), pkg_name.white(), current_ver.red());
    }

    lock.save(&lock_path)?;

    println!();
    println!("{} Lock file written: {}", "✔".red(), lock_path.display().to_string().white());
    println!("  {} packages locked", lock.packages.len());
    println!("  Commit to version control to ensure reproducible installs.");
    Ok(())
}

/// Sprawdź czy stan instalacji odpowiada lock file.
fn cmd_check(project: Option<&str>) -> Result<()> {
    let lock_path = resolve_lock_path(project);
    let lock      = LockFile::load(&lock_path)?;
    let state     = State::load()?;

    println!("{} Checking lock file consistency...", "→".white());
    println!("  Lock: {}", lock_path.display().to_string().dimmed());
    println!("  Generated: {} by hpm {}", format_ts(lock.generated_at), lock.hpm_version.dimmed());
    println!();

    let diffs = lock.check_against_state(&state);

    if diffs.is_empty() {
        println!("{} Lock file matches installed packages. Reproducible install verified.", "✔".red());
        Ok(())
    } else {
        println!("{} {} discrepancy(ies) found:\n", "✗".red(), diffs.len());
        for d in &diffs {
            println!("  {} {}", "✗".red(), d);
        }
        println!();
        println!("  Run {} to update lock, or {} to restore.",
                 "hpm lock generate".bright_black(), "hpm lock restore".bright_black());
        bail!("Lock file check failed");
    }
}

/// Pokaż status lock file.
fn cmd_status(project: Option<&str>) -> Result<()> {
    let lock_path = resolve_lock_path(project);

    if !lock_path.exists() {
        println!("{} No lock file at {}", "→".bright_black(), lock_path.display());
        println!("  Run {} to create one.", "hpm lock generate".bright_black());
        return Ok(());
    }

    let lock  = LockFile::load(&lock_path)?;
    let state = State::load()?;
    let diffs = lock.check_against_state(&state);

    println!("{} Lock file: {}", "→".white(), lock_path.display().to_string().white());
    println!("  Lock version : {}", lock.lock_version);
    println!("  hpm version  : {}", lock.hpm_version.dimmed());
    println!("  Generated    : {}", format_ts(lock.generated_at));
    println!("  Packages     : {}", lock.packages.len());
    println!();

    if diffs.is_empty() {
        println!("{} In sync with installed packages.", "✔".red());
    } else {
        println!("{} {} discrepancy(ies):", "⚠".bright_black(), diffs.len());
        for d in &diffs {
            println!("  {} {}", "⚠".bright_black(), d);
        }
    }

    Ok(())
}

/// Pokaż różnicę między lock a stanem.
fn cmd_diff(project: Option<&str>) -> Result<()> {
    let lock_path = resolve_lock_path(project);
    let lock      = LockFile::load(&lock_path)?;
    let state     = State::load()?;
    let diffs     = lock.check_against_state(&state);

    if diffs.is_empty() {
        println!("{} No differences — lock matches state.", "✔".red());
    } else {
        println!("{} Differences (lock ↔ state):\n", "→".bright_black());
        for d in &diffs {
            let (prefix, color) = if d.starts_with("missing") || d.starts_with("checksum") {
                ("−", "\033[0;31m")
            } else if d.starts_with("unlocked") {
                ("+", "\033[0;32m")
            } else {
                ("~", "\033[0;33m")
            };
            println!("  {}{} {}\033[0m", color, prefix, d);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pobierz SHA commit z lokalnie sklonowanego repo.
fn get_git_commit(pkg_name: &str, version: &str) -> String {
    let repos_dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hpm/repos")
        .join(pkg_name);

    if !repos_dir.exists() { return "unknown".to_string(); }

    let repo = match git2::Repository::open(&repos_dir) {
        Ok(r)  => r,
        Err(_) => return "unknown".to_string(),
    };

    // Szukaj tagu dla tej wersji
    if let Ok(tags) = repo.tag_names(None) {
        for tag in tags.iter().flatten() {
            if tag.trim_start_matches('v') == version {
                if let Ok(obj) = repo.revparse_single(tag) {
                    if let Ok(commit) = obj.peel_to_commit() {
                        return commit.id().to_string();
                    }
                }
            }
        }
    }

    // Fallback: HEAD
    repo.head().ok()
        .and_then(|h| h.peel_to_commit().ok())
        .map(|c| c.id().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_ts(ts: u64) -> String {
    let h   = (ts % 86400) / 3600;
    let min = (ts % 3600) / 60;
    let days = ts / 86400;
    let z   = days as i64 + 719468;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y   = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, m, d, h, min)
}

fn print_help() {
    println!("\n{} {}\n", "hpm lock".bold().red(), "— Reproducible install lock file");
    println!("{}  hpm lock {} [project_dir]\n", "Usage:".bold(), "<subcommand>".bright_black());
    println!("{}", "Subcommands:".bold().underline());
    println!("  {:<20} {}", "generate".red(), "Generate/update hpm.lock from current installs");
    println!("  {:<20} {}", "check".red(),    "Verify state matches lock file (exits 1 if not)");
    println!("  {:<20} {}", "status".red(),   "Show lock file info and sync status");
    println!("  {:<20} {}", "diff".red(),     "Show differences between lock and installed state");
    println!();
    println!("{}", "Files:".bold().underline());
    println!("  ./hpm.lock                Local lock (when info.hk present in CWD)");
    println!("  /var/lib/hpm/system.lock  Global system lock");
    println!();
    println!("{}", "CI usage:".bold().underline());
    println!("  {}", "hpm lock check  # exits 1 if state diverged from lock".dimmed());
    println!();
}
