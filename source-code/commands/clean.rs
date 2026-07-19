use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::Path;
use crate::state::State;

pub fn clean_cache() -> Result<()> {
    clean_internal(false)
}

pub fn clean_all() -> Result<()> {
    clean_internal(true)
}

fn clean_internal(also_store: bool) -> Result<()> {
    let mut removed_repos  = 0usize;
    let mut removed_files  = 0usize;
    let mut freed_bytes:u64 = 0;

    // ── 1. Cache git repo (~/.cache/hpm/repos/) ──────────────────────────────
    let repos_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("hpm/repos");
    if repos_dir.exists() {
        for entry in fs::read_dir(&repos_dir).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            let path  = entry.path();
            if path.is_dir() {
                freed_bytes += dir_size(&path);
                fs::remove_dir_all(&path).into_diagnostic()?;
                removed_repos += 1;
            }
        }
    }

    // ── 2. Legacy /var/cache/hpm ─────────────────────────────────────────────
    let cache_dir = Path::new(crate::cache_dir());
    if cache_dir.exists() {
        for entry in fs::read_dir(cache_dir).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            let path  = entry.path();
            if path.is_file() {
                freed_bytes += path.metadata().map(|m| m.len()).unwrap_or(0);
                fs::remove_file(&path).into_diagnostic()?;
                removed_files += 1;
            }
        }
    }

    // ── 3. Staging dirs (jeśli zostały po przerwanej instalacji) ─────────────
    let store_path = Path::new(crate::store_path());
    let mut removed_staging = 0usize;
    if store_path.exists() {
        if let Ok(pkg_entries) = fs::read_dir(store_path) {
            for pkg_entry in pkg_entries.flatten() {
                if !pkg_entry.path().is_dir() { continue; }
                if let Ok(ver_entries) = fs::read_dir(pkg_entry.path()) {
                    for ver_entry in ver_entries.flatten() {
                        let name = ver_entry.file_name().to_string_lossy().to_string();
                        if name.starts_with(".staging-") {
                            freed_bytes += dir_size(&ver_entry.path());
                            let _ = fs::remove_dir_all(ver_entry.path());
                            removed_staging += 1;
                        }
                    }
                }
            }
        }
    }
    if removed_staging > 0 {
        println!("{} Cleaned {} leftover staging dir(s) (from interrupted installs).",
                 "✔".red(), removed_staging);
    }

    // ── 4. Store starych wersji (--all) ───────────────────────────────────────
    if also_store {
        let mut state = State::load()?;
        clean_old_store_versions(&mut state, &mut freed_bytes)?;
    }

    if removed_repos == 0 && removed_files == 0 && removed_staging == 0 && !also_store {
        println!("{} Cache is already empty.", "✔".red());
    } else {
        println!("{} Cleaned: {} git repo(s), {} file(s) — {} freed.",
                 "✔".red(), removed_repos, removed_files, human_bytes(freed_bytes));
    }
    Ok(())
}

/// Usuń stare (niebieżące) wersje pakietów ze store.
/// Zachowuje tylko wersję wskazywaną przez symlink `current`.
fn clean_old_store_versions(state: &mut State, freed: &mut u64) -> Result<()> {
    let store_path = Path::new(crate::store_path());
    if !store_path.exists() { return Ok(()); }

    let mut to_remove: Vec<(String, String, u64)> = Vec::new(); // (pkg, ver, bytes)

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        for (ver, _info) in versions {
            if ver == &current_ver { continue; }
            let ver_dir = store_path.join(pkg_name).join(ver);
            if ver_dir.exists() {
                let size = crate::squash::on_disk_size(&ver_dir);
                to_remove.push((pkg_name.clone(), ver.clone(), size));
            }
        }
    }

    if to_remove.is_empty() {
        println!("{} No old package versions in store.", "✔".red());
        return Ok(());
    }

    let total_size: u64 = to_remove.iter().map(|(_, _, s)| s).sum();
    println!("{} Old package versions to remove:\n", "→".bright_black());
    for (pkg, ver, size) in &to_remove {
        println!("  {} {}@{}  {}", "–".red(), pkg.white(), ver, human_bytes(*size).dimmed());
    }
    println!();
    println!("  Total: {}", human_bytes(total_size).bright_black());
    println!();
    eprint!("Remove {} old version(s)? [y/N] ", to_remove.len());
    std::io::stderr().flush().into_diagnostic()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("{} Aborted.", "→".bright_black());
        return Ok(());
    }

    // BUG NAPRAWIONY (znaleziony przez realny test `hpm clean --all`): kod
    // usuwał katalog wersji z dysku, ale NIGDY nie aktualizował stanu
    // (`state.rs` / `~/.hackeros/hpm/db/hpm.db`) — baza dalej "wiedziała" o
    // usuniętej już wersji, więc `hpm list`/`hpm verify` odnosiły się do
    // czegoś, co fizycznie nie istnieje. Teraz usuwamy z obu miejsc razem,
    // atomowo (jeden `state.save()` na końcu całej operacji).
    for (pkg, ver, size) in &to_remove {
        let ver_dir = store_path.join(pkg).join(ver);
        crate::squash::unmount(&ver_dir)?;
        fs::remove_dir_all(&ver_dir).into_diagnostic()?;
        crate::squash::remove_sidecar(&ver_dir)?;
        state.remove_package_version(pkg, ver);
        *freed += size;
        println!("  {} Removed {}@{}", "✔".red(), pkg.white(), ver);
    }
    state.save()?;
    println!("\n{} Removed {} old version(s), freed {}.",
             "✔".red(), to_remove.len(), human_bytes(*freed));
    Ok(())
}

pub(crate) fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path).into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    if      bytes < 1024            { format!("{} B",   bytes) }
    else if bytes < 1024 * 1024     { format!("{:.1} KB", bytes as f64 / 1024.0) }
    else if bytes < 1024*1024*1024  { format!("{:.1} MB", bytes as f64 / 1_048_576.0) }
    else                            { format!("{:.2} GB", bytes as f64 / 1_073_741_824.0) }
}
