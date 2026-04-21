use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{
    STORE_PATH,
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
    fn ok(&mut self, msg: impl Into<String>)      { self.ok.push(msg.into()); }
    fn warn(&mut self, msg: impl Into<String>)     { self.warnings.push(msg.into()); }
    fn error(&mut self, msg: impl Into<String>)    { self.errors.push(msg.into()); }
}

pub fn doctor() -> Result<()> {
    println!("{} Running hpm diagnostics...\n", "→".cyan());

    let state = State::load()?;
    let mut report = Report::default();

    // ── 1. State file ────────────────────────────────────────────────────────
    if Path::new("/var/lib/hpm/state.json").exists() {
        report.ok("state.json exists and is readable");
    } else {
        report.warn("state.json does not exist yet (no packages installed)");
    }

    // ── 2. Store directory ───────────────────────────────────────────────────
    let store_path = Path::new(STORE_PATH);
    if !store_path.exists() {
        report.warn(format!("Store directory {} does not exist", STORE_PATH));
    } else {
        report.ok(format!("Store directory {} exists", STORE_PATH));
    }

    // ── 3. Per-package checks ────────────────────────────────────────────────
    for (pkg_name, versions) in &state.packages {
        let pkg_store_dir = store_path.join(pkg_name);

        // Check current symlink
        let current_link = pkg_store_dir.join("current");
        let current_ver = if current_link.exists() {
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

            // Does the version directory exist?
            if !ver_dir.exists() {
                report.error(format!("{}@{}: directory missing from store ({})",
                                     pkg_name, ver, ver_dir.display()));
                continue;
            }

            // Checksum verification
            match compute_dir_hash(&ver_dir) {
                Ok(actual) => {
                    if actual == info.checksum {
                        report.ok(format!("{}@{}: checksum OK", pkg_name, ver));
                    } else {
                        report.error(format!(
                            "{}@{}: checksum MISMATCH\n    stored:   {}\n    computed: {}",
                            pkg_name, ver, &info.checksum[..12], &actual[..12]
                        ));
                    }
                }
                Err(e) => {
                    report.warn(format!("{}@{}: could not compute checksum: {}", pkg_name, ver, e));
                }
            }

            // Check /usr/bin wrappers
            if let Ok(manifest) = crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap()) {
                for bin_name in &manifest.bins {
                    let wrapper = Path::new("/usr/bin").join(bin_name);
                    if !wrapper.exists() {
                        if current_ver.as_deref() == Some(ver.as_str()) {
                            report.error(format!(
                                "{}@{}: /usr/bin/{} wrapper missing (package is current but has no wrapper)",
                                                 pkg_name, ver, bin_name
                            ));
                        } else {
                            report.warn(format!(
                                "{}@{}: /usr/bin/{} wrapper missing (non-current version, OK)",
                                                pkg_name, ver, bin_name
                            ));
                        }
                    } else {
                        // Check wrapper content
                        let content = fs::read_to_string(&wrapper).unwrap_or_default();
                        if content.contains(&format!("hpm run {} ", pkg_name)) {
                            report.ok(format!("{}@{}: /usr/bin/{} wrapper OK", pkg_name, ver, bin_name));
                        } else {
                            report.warn(format!(
                                "{}@{}: /usr/bin/{} wrapper exists but doesn't call hpm run",
                                pkg_name, ver, bin_name
                            ));
                        }
                    }
                }

                // Check .desktop for GUI apps
                if manifest.is_gui || manifest.sandbox.gui {
                    let desktop = Path::new("/usr/share/applications")
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

    // ── 4. Orphaned store directories (on disk but not in state) ─────────────
    if store_path.exists() {
        if let Ok(rd) = fs::read_dir(store_path) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !state.packages.contains_key(&name) {
                    report.warn(format!(
                        "Store directory {}/{} exists but is not in state.json (orphaned store entry)",
                                        STORE_PATH, name
                    ));
                }
            }
        }
    }

    // ── 5. Stale wrappers (wrapper in /usr/bin but package not installed) ────
    if let Ok(rd) = fs::read_dir("/usr/bin") {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_file() {
                let content = fs::read_to_string(&path).unwrap_or_default();
                if content.contains("hpm run ") {
                    // Extract package name from wrapper
                    if let Some(pkg) = extract_pkg_from_wrapper(&content) {
                        if !state.packages.contains_key(&pkg) {
                            report.warn(format!(
                                "/usr/bin/{}: wrapper references '{}' but it is not installed",
                                path.file_name().unwrap_or_default().to_string_lossy(),
                                                pkg
                            ));
                        }
                    }
                }
            }
        }
    }

    // ── Print report ─────────────────────────────────────────────────────────
    println!("{} Summary:\n", "→".cyan());

    for msg in &report.ok {
        println!("  {} {}", "✔".green(), msg);
    }
    for msg in &report.warnings {
        println!("  {} {}", "⚠".yellow(), msg);
    }
    for msg in &report.errors {
        println!("  {} {}", "✗".red(), msg);
    }

    println!();
    println!("  Checks:   {}", report.ok.len() + report.warnings.len() + report.errors.len());
    println!("  {} OK:       {}", "✔".green(), report.ok.len());
    println!("  {} Warnings: {}", "⚠".yellow(), report.warnings.len());
    println!("  {} Errors:   {}", "✗".red(), report.errors.len());

    if !report.errors.is_empty() {
        println!("\n{} Run {} to attempt automatic repair.", "→".yellow(), "hpm repair".yellow());
    } else if report.warnings.is_empty() {
        println!("\n{} All checks passed.", "✔".green());
    }

    Ok(())
}

fn extract_pkg_from_wrapper(content: &str) -> Option<String> {
    // Wrapper format: exec /path/to/hpm run <pkg> <bin>
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("exec ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            // parts: [hpm_path, "run", pkg_name, bin_name]
            if parts.len() >= 3 && parts[1] == "run" {
                return Some(parts[2].to_string());
            }
        }
    }
    None
}
