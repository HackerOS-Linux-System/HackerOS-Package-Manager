use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::Path;
use crate::{
    state::State,
    utils::compute_dir_hash,
};

#[derive(Default)]
struct Report {
    ok:       Vec<String>,
    warnings: Vec<String>,
    errors:   Vec<String>,
}

impl Report {
    fn ok(&mut self, msg: impl Into<String>)   { self.ok.push(msg.into()); }
    fn warn(&mut self, msg: impl Into<String>)  { self.warnings.push(msg.into()); }
    fn error(&mut self, msg: impl Into<String>) { self.errors.push(msg.into()); }
}

// ---------------------------------------------------------------------------
// Diagnose
// ---------------------------------------------------------------------------

pub fn doctor() -> Result<()> {
    println!("{} Running hpm diagnostics...\n", "→".white());

    let state      = State::load()?;
    let mut report = Report::default();

    // ── 1. State database ────────────────────────────────────────────────────
    if Path::new(&crate::db::db_path()).exists() {
        report.ok("hpm.db exists and is readable");
    } else {
        report.warn("hpm.db does not exist yet (no packages installed)");
    }

    // ── 2. Store directory ───────────────────────────────────────────────────
    let store_path = Path::new(crate::store_path());
    if !store_path.exists() {
        report.warn(format!("Store directory {} does not exist", crate::store_path()));
    } else {
        report.ok(format!("Store directory {} exists", crate::store_path()));
    }

    // ── 3. Per-package checks ────────────────────────────────────────────────
    for (pkg_name, versions) in &state.packages {
        let pkg_store_dir = store_path.join(pkg_name);

        let current_link = pkg_store_dir.join("current");
        let current_ver  = if current_link.exists() {
            match fs::read_link(&current_link) {
                Ok(target) => {
                    let ver = target.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_string();
                    report.ok(format!("{}: current → {}", pkg_name, ver));
                    Some(ver)
                }
                Err(e) => {
                    report.error(format!("{}: current symlink unreadable: {}", pkg_name, e));
                    None
                }
            }
        } else {
            report.error(format!("{}: missing current symlink", pkg_name));
            None
        };

        for (ver, info) in versions {
            let ver_dir = pkg_store_dir.join(ver);

            if !ver_dir.exists() {
                report.error(format!("{}@{}: directory missing from store ({})",
                                     pkg_name, ver, ver_dir.display()));
                continue;
            }
            if let Err(e) = crate::squash::ensure_mounted(&ver_dir) {
                report.error(format!("{}@{}: could not mount compressed store: {}", pkg_name, ver, e));
                continue;
            }

            match compute_dir_hash(&ver_dir) {
                Ok(actual) => {
                    if actual == info.checksum {
                        report.ok(format!("{}@{}: checksum OK", pkg_name, ver));
                    } else {
                        report.error(format!(
                            "{}@{}: checksum MISMATCH\n    stored:   {}\n    computed: {}",
                            pkg_name, ver, &info.checksum[..12.min(info.checksum.len())],
                            &actual[..12.min(actual.len())]
                        ));
                    }
                }
                Err(e) => {
                    report.warn(format!("{}@{}: could not compute checksum: {}", pkg_name, ver, e));
                }
            }

            if let Ok(manifest) = crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap()) {
                for bin_name in &manifest.bins {
                    let wrapper = Path::new(crate::bin_dir()).join(bin_name);
                    if !wrapper.exists() {
                        // Też sprawdź alternatywną nazwę pkg-bin
                        let alt_wrapper = Path::new(crate::bin_dir()).join(format!("{}-{}", pkg_name, bin_name));
                        if alt_wrapper.exists() {
                            report.warn(format!(
                                "{}@{}: {bin_dir}/{} missing but {bin_dir}/{}-{} exists (renamed wrapper)",
                                pkg_name, ver, bin_name, pkg_name, bin_name, bin_dir = crate::bin_dir()
                            ));
                        } else if current_ver.as_deref() == Some(ver.as_str()) {
                            report.error(format!(
                                "{}@{}: {}/{} wrapper missing (package is current but has no wrapper)",
                                pkg_name, ver, crate::bin_dir(), bin_name
                            ));
                        } else {
                            report.warn(format!(
                                "{}@{}: {}/{} wrapper missing (non-current version, OK)",
                                pkg_name, ver, crate::bin_dir(), bin_name
                            ));
                        }
                    } else {
                        let content = fs::read_to_string(&wrapper).unwrap_or_default();
                        if content.contains(&format!("hpm run {} ", pkg_name)) {
                            report.ok(format!("{}@{}: {}/{} wrapper OK", pkg_name, ver, crate::bin_dir(), bin_name));
                        } else {
                            report.warn(format!(
                                "{}@{}: {}/{} wrapper exists but doesn't call hpm run",
                                pkg_name, ver, crate::bin_dir(), bin_name
                            ));
                        }
                    }
                }

                if manifest.is_gui || manifest.sandbox.gui {
                    let desktop = Path::new(crate::desktop_dir())
                        .join(format!("{}.desktop", pkg_name));
                    if !desktop.exists() {
                        report.warn(format!(
                            "{}: GUI app but no .desktop file at {}",
                            pkg_name, desktop.display()
                        ));
                    } else {
                        report.ok(format!("{}: .desktop file present", pkg_name));
                    }
                }
            } else {
                report.warn(format!("{}@{}: could not read info.hk from store", pkg_name, ver));
            }
        }
    }

    // ── 4. Orphaned store directories ────────────────────────────────────────
    if store_path.exists() {
        if let Ok(rd) = fs::read_dir(store_path) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !state.packages.contains_key(&name) {
                    report.warn(format!(
                        "Store directory {}/{} exists but is not in state.json (orphaned store entry)",
                        crate::store_path(), name
                    ));
                }
            }
        }
    }

    // ── 5. Stale wrappers ────────────────────────────────────────────────────
    if let Ok(rd) = fs::read_dir(crate::bin_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_file() {
                let content = fs::read_to_string(&path).unwrap_or_default();
                if content.contains("hpm run ") {
                    if let Some(pkg) = extract_pkg_from_wrapper(&content) {
                        if !state.packages.contains_key(&pkg) {
                            report.warn(format!(
                                "{}/{}: wrapper references '{}' but it is not installed",
                                crate::bin_dir(),
                                path.file_name().unwrap_or_default().to_string_lossy(),
                                pkg
                            ));
                        }
                    }
                }
            }
        }
    }

    // ── 6. Duplicate wrapper names ───────────────────────────────────────────
    check_duplicate_wrappers(&state, &mut report);

    // ── Print report ─────────────────────────────────────────────────────────
    println!("{} Summary:\n", "→".white());

    for msg in &report.ok       { println!("  {} {}", "✔".red(),  msg); }
    for msg in &report.warnings { println!("  {} {}", "⚠".bright_black(), msg); }
    for msg in &report.errors   { println!("  {} {}", "✗".red(),    msg); }

    println!();
    println!("  Checks:   {}", report.ok.len() + report.warnings.len() + report.errors.len());
    println!("  {} OK:       {}", "✔".red(),  report.ok.len());
    println!("  {} Warnings: {}", "⚠".bright_black(), report.warnings.len());
    println!("  {} Errors:   {}", "✗".red(),    report.errors.len());

    if !report.errors.is_empty() {
        println!("\n{} Run {} to attempt automatic repair.", "→".bright_black(), "hpm repair".bright_black());
    } else if report.warnings.is_empty() {
        println!("\n{} All checks passed.", "✔".red());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

pub fn repair() -> Result<()> {
    println!("{} Running hpm repair...\n", "→".white());

    let state    = State::load()?;
    let hpm_exe  = crate::commands::install::which_hpm_for_wrappers();
    let store    = Path::new(crate::store_path());
    let mut fixed = 0usize;

    // ── 1. Napraw brakujące symlinki current ─────────────────────────────────
    for (pkg_name, versions) in &state.packages {
        let pkg_dir = store.join(pkg_name);
        let link    = pkg_dir.join("current");

        if !link.exists() {
            // Wybierz najnowszą zainstalowaną wersję
            let mut vers: Vec<&String> = versions.keys().collect();
            vers.sort_by(|a, b| crate::utils::compare_versions(a, b));
            if let Some(newest) = vers.last() {
                let target = pkg_dir.join(newest);
                if target.exists() {
                    std::os::unix::fs::symlink(newest, &link).into_diagnostic()?;
                    println!("  {} Fixed missing 'current' symlink for {} → {}", 
                             "✔".red(), pkg_name.white(), newest.red());
                    fixed += 1;
                }
            }
        }
    }

    // ── 2. Napraw brakujące wrappery bin_dir() ──────────────────────────────
    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };

        let ver_dir = store.join(pkg_name).join(&current_ver);
        if !ver_dir.exists() { continue; }
        if crate::squash::ensure_mounted(&ver_dir).is_err() { continue; }

        let manifest = match crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap()) {
            Ok(m)  => m,
            Err(_) => continue,
        };

        for bin_name in &manifest.bins {
            let wrapper = Path::new(crate::bin_dir()).join(bin_name);

            // Sprawdź czy wrapper jest poprawny
            let needs_fix = if wrapper.exists() {
                let content = fs::read_to_string(&wrapper).unwrap_or_default();
                !content.contains(&format!("hpm run {} ", pkg_name))
            } else {
                true
            };

            if needs_fix {
                let bin_rel = if let Some(explicit) = manifest.bin_paths.get(bin_name) {
                    let p = ver_dir.join(explicit);
                    if p.exists() { Some(explicit.clone()) } else { None }
                } else {
                    crate::commands::install::find_binary_in_dir(&ver_dir, bin_name)
                };

                if let Some(rel) = bin_rel {
                    let content = format!(
                        "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
                        hpm_exe, pkg_name, rel
                    );
                    fs::write(&wrapper, &content).into_diagnostic()?;
                    crate::utils::make_executable(&wrapper)?;
                    println!("  {} Repaired wrapper {}/{} for {}@{}",
                             "✔".red(), crate::bin_dir(), bin_name.white(), pkg_name, current_ver);
                    fixed += 1;
                } else {
                    println!("  {} Cannot repair {}/{}: binary not found in store",
                             "⚠".bright_black(), crate::bin_dir(), bin_name);
                }
            }
        }
    }

    // ── 3. Usuń osierocone wrappery ───────────────────────────────────────────
    if let Ok(rd) = fs::read_dir(crate::bin_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }
            let content = fs::read_to_string(&path).unwrap_or_default();
            if content.contains("hpm run ") {
                if let Some(pkg) = extract_pkg_from_wrapper(&content) {
                    if !state.packages.contains_key(&pkg) {
                        fs::remove_file(&path).into_diagnostic()?;
                        println!("  {} Removed stale wrapper {} (package '{}' not installed)",
                                 "✔".red(),
                                 path.file_name().unwrap_or_default().to_string_lossy().white(),
                                 pkg);
                        fixed += 1;
                    }
                }
            }
        }
    }

    // ── 4. Napraw .desktop dla GUI aplikacji ──────────────────────────────────
    for (pkg_name, _versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        let ver_dir = store.join(pkg_name).join(&current_ver);
        if !ver_dir.exists() { continue; }
        if crate::squash::ensure_mounted(&ver_dir).is_err() { continue; }

        let manifest = match crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap()) {
            Ok(m)  => m,
            Err(_) => continue,
        };

        if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
            let desktop = Path::new(crate::desktop_dir())
                .join(format!("{}.desktop", pkg_name));
            if !desktop.exists() {
                match crate::commands::install::install_desktop_integration_pub(
                    &ver_dir, &manifest, pkg_name, &hpm_exe.to_string(),
                ) {
                    Ok(_)  => {
                        println!("  {} Restored .desktop for {}", "✔".red(), pkg_name.white());
                        fixed += 1;
                    }
                    Err(e) => {
                        println!("  {} Could not restore .desktop for {}: {}", 
                                 "⚠".bright_black(), pkg_name, e);
                    }
                }
            }
        }
    }

    if fixed == 0 {
        println!("{} Nothing needed repair.", "✔".red());
    } else {
        println!("\n{} Repaired {} issue(s).", "✔".red(), fixed);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn check_duplicate_wrappers(state: &State, report: &mut Report) {
    use std::collections::HashMap;
    let mut wrapper_owners: HashMap<String, Vec<String>> = HashMap::new();

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        let ver_dir = Path::new(crate::store_path()).join(pkg_name).join(&current_ver);
        let _ = crate::squash::ensure_mounted(&ver_dir);
        if let Ok(manifest) = crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap_or("")) {
            for bin_name in &manifest.bins {
                wrapper_owners
                    .entry(bin_name.clone())
                    .or_default()
                    .push(pkg_name.clone());
            }
        }
    }

    for (bin_name, owners) in &wrapper_owners {
        if owners.len() > 1 {
            report.warn(format!(
                "{}/{} is claimed by multiple packages: {}",
                crate::bin_dir(),
                bin_name,
                owners.join(", ")
            ));
        }
    }
}

fn extract_pkg_from_wrapper(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("exec ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3 && parts[1] == "run" {
                return Some(parts[2].to_string());
            }
        }
    }
    None
}
