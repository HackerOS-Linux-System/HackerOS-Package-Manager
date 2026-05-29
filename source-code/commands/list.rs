use miette::Result;
use colored::Colorize;
use std::path::Path;
use crate::{STORE_PATH, state::State};

pub fn list_installed() -> Result<()> {
    let state = State::load()?;
    if state.packages.is_empty() {
        println!("{} No packages installed.", "→".yellow());
        return Ok(());
    }
    println!("{} Installed packages:", "→".blue());
    println!("  {:<20} {:<15} {:<8} {}", "Package".cyan(), "Version".cyan(), "Pinned".cyan(), "Tags".cyan());
    for (pkg_name, versions) in &state.packages {
        let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
        let current_ver  = if current_link.exists() {
            current_link.read_link().ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default()
        } else { String::new() };

        for (ver, info) in versions {
            let is_current = ver == &current_ver;
            let pinned     = if info.pinned { "✓".green() } else { "✗".red() };
            // Pobierz tagi z cache
            let tags_str = if let Some(meta) = crate::repo::load_cached_meta_pub(pkg_name) {
                if meta.tags.is_empty() { String::new() }
                else { meta.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(" ") }
            } else { String::new() };

            println!(
                "  {:<20} {:<15} {:<8} {} {}",
                if is_current { pkg_name.magenta().to_string() } else { pkg_name.to_string() },
                if is_current { ver.cyan().to_string() } else { ver.to_string() },
                pinned,
                if is_current { "(current)".yellow().to_string() } else { String::new() },
                tags_str.dimmed()
            );
        }
    }
    Ok(())
}
