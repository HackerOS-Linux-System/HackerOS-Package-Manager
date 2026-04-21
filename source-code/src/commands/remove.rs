use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::Path;
use crate::{
    STORE_PATH,
    state::State,
    utils::{acquire_lock, release_lock},
};

pub fn remove(spec: String) -> Result<()> {
    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let mut state = State::load()?;

    let (pkg_name, version) = if spec.contains('@') {
        let mut parts = spec.splitn(2, '@');
        (parts.next().unwrap().to_string(), Some(parts.next().unwrap().to_string()))
    } else {
        (spec.clone(), None)
    };

    if !state.packages.contains_key(&pkg_name) {
        bail!("Package '{}' is not installed", pkg_name);
    }

    // ── Reverse dependency check ─────────────────────────────────────────────
    let rdeps = state.reverse_deps(&pkg_name);
    if !rdeps.is_empty() {
        // Filter out the version being removed if only removing one version
        let remaining_rdeps: Vec<&String> = if let Some(ref ver) = version {
            let removing_key = format!("{}@{}", pkg_name, ver);
            rdeps.iter()
            .filter(|dep| {
                // Only warn about packages that depend on THIS specific version
                // or any version of the package if no other version remains
                let other_versions_exist = state.packages.get(&pkg_name)
                .map(|vs| vs.len() > 1)
                .unwrap_or(false);
                !other_versions_exist || dep.as_str() != removing_key.as_str()
            })
            .collect()
        } else {
            rdeps.iter().collect()
        };

        if !remaining_rdeps.is_empty() {
            eprintln!("{} The following packages depend on {}:", "⚠".yellow(), pkg_name.cyan());
            for dep in &remaining_rdeps {
                eprintln!("  {} {}", "→".yellow(), dep);
            }
            eprint!("Remove anyway? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).into_diagnostic()?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("{} Aborted.", "→".yellow());
                return Ok(());
            }
        }
    }

    // Snapshot before removal
    state.push_snapshot(&format!("pre-remove {}", spec));

    if let Some(ver) = version {
        remove_version(&pkg_name, &ver, &mut state)?;
        println!("{} {}@{} removed", "✔".green(), pkg_name.cyan(), ver.cyan());
    } else {
        let versions: Vec<String> = state.packages.get(&pkg_name)
        .unwrap().keys().cloned().collect();
        for ver in &versions {
            remove_version(&pkg_name, ver, &mut state)?;
        }
        println!("{} {} removed", "✔".green(), pkg_name.cyan());
    }

    state.save()?;
    Ok(())
}

pub fn remove_version(pkg_name: &str, version: &str, state: &mut State) -> Result<()> {
    let pkg_path = Path::new(STORE_PATH).join(pkg_name).join(version);
    if !pkg_path.exists() {
        bail!("Path {} does not exist", pkg_path.display());
    }

    // Remove /usr/bin wrappers (read manifest from store)
    if let Ok(manifest) = crate::manifest::Manifest::load_from_path(pkg_path.to_str().unwrap()) {
        for bin in &manifest.bins {
            let wrapper = Path::new("/usr/bin").join(bin);
            if wrapper.exists() {
                fs::remove_file(&wrapper).into_diagnostic()?;
            }
        }
        // Remove .desktop file and icon for GUI apps
        if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
            remove_desktop_integration(pkg_name)?;
        }
    }

    fs::remove_dir_all(&pkg_path).into_diagnostic()?;
    state.remove_package_version(pkg_name, version);

    // Update current symlink if we just removed the current version
    let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
    if let Ok(target) = fs::read_link(&current_link) {
        if target == Path::new(version) {
            fs::remove_file(&current_link).into_diagnostic()?;
            // Point current to another installed version if one exists
            if let Some(vers) = state.packages.get(pkg_name) {
                let mut remaining: Vec<&String> = vers.keys().collect();
                remaining.sort_by(|a, b| crate::utils::compare_versions(a, b));
                if let Some(newest) = remaining.last() {
                    std::os::unix::fs::symlink(newest, &current_link).into_diagnostic()?;
                    println!("  {} Switched current to {}", "→".yellow(), newest.cyan());
                }
            }
        }
    }

    Ok(())
}

fn remove_desktop_integration(pkg_name: &str) -> Result<()> {
    let desktop = Path::new("/usr/share/applications")
    .join(format!("{}.desktop", pkg_name));
    if desktop.exists() {
        fs::remove_file(&desktop).into_diagnostic()?;
    }

    // Remove icons
    for size in &["16x16", "32x32", "48x48", "64x64", "128x128", "256x256", "scalable"] {
        for ext in &["png", "svg", "xpm"] {
            let icon = Path::new("/usr/share/icons/hicolor")
            .join(format!("{}/apps", size))
            .join(format!("{}.{}", pkg_name, ext));
            if icon.exists() {
                let _ = fs::remove_file(&icon);
            }
        }
    }
    for ext in &["png", "svg", "xpm"] {
        let pixmap = Path::new("/usr/share/pixmaps")
        .join(format!("{}.{}", pkg_name, ext));
        if pixmap.exists() {
            let _ = fs::remove_file(&pixmap);
        }
    }

    let _ = std::process::Command::new("update-desktop-database")
    .arg("/usr/share/applications").status();
    let _ = std::process::Command::new("gtk-update-icon-cache")
    .args(["-f", "-t", "/usr/share/icons/hicolor"]).status();

    Ok(())
}
