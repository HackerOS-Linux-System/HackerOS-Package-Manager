use miette::Result;
use colored::Colorize;
use std::path::Path;
use crate::{state::State};

pub fn list_installed() -> Result<()> {
    let state = State::load()?;
    if state.packages.is_empty() {
        println!("{} No packages installed.", "→".bright_black());
        return Ok(());
    }
    println!("{} Installed packages:", "→".white());
    println!("  {:<20} {:<15} {:<8} {}", "Package".white(), "Version".white(), "Pinned".white(), "Tags".white());
    for (pkg_name, versions) in &state.packages {
        let current_link = Path::new(crate::store_path()).join(pkg_name).join("current");
        let current_ver  = if current_link.exists() {
            current_link.read_link().ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default()
        } else { String::new() };

        for (ver, info) in versions {
            let is_current = ver == &current_ver;
            let pinned     = if info.pinned { "✓".red() } else { "✗".red() };
            // Pobierz tagi z cache
            let tags_str = if let Some(meta) = crate::repo::load_cached_meta_pub(pkg_name) {
                if meta.tags.is_empty() { String::new() }
                else { meta.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(" ") }
            } else { String::new() };

            println!(
                "  {:<20} {:<15} {:<8} {} {}",
                if is_current { pkg_name.magenta().to_string() } else { pkg_name.to_string() },
                if is_current { ver.white().to_string() } else { ver.to_string() },
                pinned,
                if is_current { "(current)".bright_black().to_string() } else { String::new() },
                tags_str.dimmed()
            );
        }
    }
    Ok(())
}
