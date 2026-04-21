use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use crate::{
    STORE_PATH,
    state::State,
    utils::{acquire_lock, release_lock},
};

pub fn rollback(package: Option<String>) -> Result<()> {
    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let mut state = State::load()?;

    if let Some(pkg_name) = package {
        // Roll back a single package to its previous version
        rollback_single_package(&pkg_name, &mut state)?;
    } else {
        // Roll back entire system state
        rollback_full_state(&mut state)?;
    }

    state.save()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Single package rollback
// ---------------------------------------------------------------------------

fn rollback_single_package(pkg_name: &str, state: &mut State) -> Result<()> {
    let prev_ver = state.get_previous_version(pkg_name)
    .ok_or_else(|| miette::miette!(
        "No previous version found for '{}'.\n\
Only one version is installed — nothing to roll back to.",
pkg_name
    ))?;

    let current_ver = state.get_current_version(pkg_name)
    .unwrap_or_default();

    println!("{} Rolling back {} from {} to {}",
             "→".yellow(), pkg_name.cyan(), current_ver.cyan(), prev_ver.cyan());

    let prev_dir = Path::new(STORE_PATH).join(pkg_name).join(&prev_ver);
    if !prev_dir.exists() {
        return Err(miette::miette!(
            "Version {}@{} files not found in store.\n\
The version is in state.json but its directory is missing.\n\
Run {} to diagnose.",
pkg_name, prev_ver, "hpm doctor".yellow()
        ));
    }

    // Update current symlink
    let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
    let _ = fs::remove_file(&current_link);
    std::os::unix::fs::symlink(&prev_ver, &current_link).into_diagnostic()?;

    // Rebuild /usr/bin wrappers for the previous version
    let hpm_exe = std::env::current_exe().into_diagnostic()?;
    if let Ok(manifest) = crate::manifest::Manifest::load_from_path(prev_dir.to_str().unwrap()) {
        for bin_name in &manifest.bins {
            let wrapper_path = Path::new("/usr/bin").join(bin_name);
            if let Some(rel) = crate::commands::install::find_binary_in_dir(&prev_dir, bin_name) {
                let content = format!(
                    "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
                    hpm_exe.display(), pkg_name, rel
                );
                fs::write(&wrapper_path, &content).into_diagnostic()?;
                crate::utils::make_executable(&wrapper_path)?;
            }
        }
    }

    println!("{} {}@{} is now the current version", "✔".green(), pkg_name.cyan(), prev_ver.cyan());
    Ok(())
}

// ---------------------------------------------------------------------------
// Full state rollback from snapshot history
// ---------------------------------------------------------------------------

fn rollback_full_state(state: &mut State) -> Result<()> {
    let history = state.list_history();

    if history.is_empty() {
        println!("{} No rollback history available.", "→".yellow());
        println!("  History is recorded automatically before install/update/remove operations.");
        return Ok(());
    }

    // Display history
    println!("{} Rollback history (most recent last):\n", "→".cyan());
    for (i, timestamp, desc) in &history {
        let dt = format_timestamp(*timestamp);
        println!("  [{}]  {}  {}", i.to_string().cyan(), dt.dimmed(), desc);
    }

    println!();
    eprint!("Enter snapshot index to restore (or 'q' to quit): ");
    std::io::stdout().flush().into_diagnostic()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    let input = input.trim();

    if input.eq_ignore_ascii_case("q") || input.is_empty() {
        println!("{} Aborted.", "→".yellow());
        return Ok(());
    }

    let index: usize = input.parse().map_err(|_| miette::miette!("Invalid index: {}", input))?;

    if index >= history.len() {
        return Err(miette::miette!("Index {} out of range (0..{})", index, history.len() - 1));
    }

    let snapshot_desc = history[index].2.to_string();
    println!("\n{} Restoring to: {}", "→".yellow(), snapshot_desc);

    // Compute diff
    let target_snapshot = state.history[index].snapshot.clone();
    let current_pkgs = state.packages.clone();

    let to_install: Vec<(String, String)> = target_snapshot.iter()
    .flat_map(|(name, vers)| {
        vers.keys().filter_map(move |ver| {
            if current_pkgs.get(name).map(|vs| vs.contains_key(ver)).unwrap_or(false) {
                None
            } else {
                Some((name.clone(), ver.clone()))
            }
        })
    })
    .collect();

    let to_remove: Vec<(String, String)> = current_pkgs.iter()
    .flat_map(|(name, vers)| {
        vers.keys().filter_map(move |ver| {
            if target_snapshot.get(name).map(|vs| vs.contains_key(ver)).unwrap_or(false) {
                None
            } else {
                Some((name.clone(), ver.clone()))
            }
        })
    })
    .collect();

    if to_install.is_empty() && to_remove.is_empty() {
        println!("{} Current state matches the selected snapshot. Nothing to do.", "✔".green());
        return Ok(());
    }

    if !to_install.is_empty() {
        println!("\n  Packages to reinstall:");
        for (name, ver) in &to_install {
            println!("    {} {}@{}", "+".green(), name.cyan(), ver);
        }
    }
    if !to_remove.is_empty() {
        println!("\n  Packages to remove:");
        for (name, ver) in &to_remove {
            println!("    {} {}@{}", "–".red(), name.cyan(), ver);
        }
    }

    println!();
    eprint!("Proceed? [y/N] ");
    std::io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("{} Aborted.", "→".yellow());
        return Ok(());
    }

    // Remove packages not in target
    for (name, ver) in &to_remove {
        let pkg_path = Path::new(STORE_PATH).join(name).join(ver);
        if pkg_path.exists() {
            crate::commands::remove::remove_version(name, ver, state)?;
            println!("  {} Removed {}@{}", "✔".green(), name.cyan(), ver);
        }
    }

    // Restore state to snapshot (files for to_install must already be in store)
    for (name, ver) in &to_install {
        let pkg_path = Path::new(STORE_PATH).join(name).join(ver);
        if pkg_path.exists() {
            // Restore current symlink
            let current_link = Path::new(STORE_PATH).join(name).join("current");
            let _ = fs::remove_file(&current_link);
            std::os::unix::fs::symlink(ver, &current_link).into_diagnostic()?;
            println!("  {} Restored {}@{} (files already in store)", "✔".green(), name.cyan(), ver);
        } else {
            println!("  {} {}@{} not in store — run {} to reinstall",
                     "⚠".yellow(), name.cyan(), ver,
                     format!("hpm install {}@{}", name, ver).yellow());
        }
    }

    // Apply snapshot to state
    state.restore_snapshot(index);

    println!("\n{} Rollback complete.", "✔".green());
    Ok(())
}

fn format_timestamp(ts: u64) -> String {
    // Simple human-readable format without chrono dependency
    let secs = ts;
    let days = secs / 86400;
    let epoch_days = 719468i64; // days from year 0 to 1970
    let z = days as i64 + epoch_days;
    let era = if z >= 0 { z / 146097 } else { (z - 146096) / 146097 };
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let h = (secs % 86400) / 3600;
    let min = (secs % 3600) / 60;
    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, h, min)
}
