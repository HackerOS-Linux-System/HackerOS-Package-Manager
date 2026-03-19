use anyhow::Result;
use colored::Colorize;
use crate::{
    STORE_PATH,
    state::State,
};
use std::path::Path;

pub fn list_installed() -> Result<()> {
    let state = State::load()?;

    if state.packages.is_empty() {
        println!("{} No packages installed.", "→".yellow());
        return Ok(());
    }

    println!("{} Installed packages:", "→".blue());
    println!("{:<20} {:<15} {}", "Package".cyan(), "Version".cyan(), "Pinned".cyan());

    for (pkg_name, versions) in &state.packages {
        let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
        let current_ver = if current_link.exists() {
            current_link.read_link()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default()
        } else {
            String::new()
        };

        for (ver, info) in versions {
            let is_current = ver == &current_ver;
            let pinned = if info.pinned { "✓".green() } else { "✗".red() };
            println!(
                "{:<20} {:<15} {} {}",
                if is_current { pkg_name.magenta() } else { pkg_name.normal() },
                    if is_current { ver.cyan() } else { ver.normal() },
                        pinned,
                     if is_current { "(current)".yellow() } else { "".clear() }
            );
        }
    }

    Ok(())
}
