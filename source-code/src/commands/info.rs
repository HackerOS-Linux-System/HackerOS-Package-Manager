use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use crate::{
    repo::RepoManager,
    state::State,
};

pub fn info(package: String) -> Result<()> {
    if package.is_empty() {
        eprintln!("{} Usage: hpm info <package>", "✗".red());
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .into_diagnostic()?;

    let repo_mgr = rt.block_on(RepoManager::load())?;
    let state = State::load()?;

    let entry = repo_mgr.index.packages.get(&package)
    .ok_or_else(|| miette::miette!(
        "Package '{}' not found in repository index.\n  Run {} to refresh.",
        package, "hpm refresh".yellow()
    ))?;

    // Fast HTTP fetch of latest info.hk
    let meta = rt.block_on(repo_mgr.fetch_package_meta(&package))?;

    // Also try to fetch build.toml summary
    let build_cfg = rt.block_on(repo_mgr.fetch_raw_build_config(&entry.repo));

    let installed_ver = state.get_current_version(&package);
    let pinned = installed_ver.as_ref()
    .and_then(|ver| state.packages.get(&package)?.get(ver))
    .map(|info| info.pinned)
    .unwrap_or(false);

    // Version list from locally cached git repo (if present)
    let local_versions: Vec<String> = {
        let repos_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("hpm/repos")
        .join(&package);

        if repos_dir.exists() {
            match git2::Repository::open(&repos_dir) {
                Ok(repo) => {
                    let mut vers = Vec::new();
                    if let Ok(tags) = repo.tag_names(None) {
                        for tag in tags.iter().flatten() {
                            vers.push(tag.trim_start_matches('v').to_string());
                        }
                    }
                    vers.sort_by(|a, b| crate::utils::compare_versions(a, b));
                    vers
                }
                Err(_) => Vec::new(),
            }
        } else {
            entry.versions.clone()
        }
    };

    // ── Output ───────────────────────────────────────────────────────────────
    println!();
    println!("  {} {}", "◆".cyan(), package.bold().cyan());
    println!("  {}", "─".repeat(60).dimmed());
    println!("  {:<14} {}", "Version:".bold(),    meta.version.green());
    println!("  {:<14} {}", "Author:".bold(),     meta.authors);
    println!("  {:<14} {}", "License:".bold(),    meta.license);
    println!("  {:<14} {}", "Repository:".bold(), entry.repo.dimmed());

    // Build type from build.toml
    if let Some(ref cfg) = build_cfg {
        let build_type = match &cfg.source {
            crate::repo::BuildSource::Download { url, .. } => {
                let trimmed = if url.len() > 50 { format!("{}…", &url[..49]) } else { url.clone() };
                format!("download ({})", trimmed.dimmed())
            }
            crate::repo::BuildSource::Build { .. } => "build from source".to_string(),
            crate::repo::BuildSource::Prebuilt   => "prebuilt (contents/)".to_string(),
        };
        println!("  {:<14} {}", "Build type:".bold(), build_type);
    }

    println!();
    println!("  {}", "Description:".bold());
    for line in wrap_text(&meta.summary, 65) {
        println!("    {}", line);
    }

    // Installed status
    println!();
    if let Some(ref ver) = installed_ver {
        let pin_tag = if pinned { format!(" {}", "(pinned)".yellow()) } else { String::new() };
        println!("  {:<14} {}{}", "Installed:".bold(), ver.cyan(), pin_tag);
    } else {
        println!("  {:<14} {}", "Installed:".bold(), "No".red());
    }

    // Available versions
    if !local_versions.is_empty() {
        println!();
        println!("  {}", "Available versions (cached):".bold());
        for v in &local_versions {
            let cur = if installed_ver.as_deref() == Some(v.as_str()) {
                format!(" {}", "← current".green())
            } else {
                String::new()
            };
            println!("    • {}{}", v.cyan(), cur);
        }
    } else if !entry.versions.is_empty() {
        println!();
        println!("  {}", "Known versions (from index):".bold());
        for v in &entry.versions {
            println!("    • {}", v.cyan());
        }
    } else {
        println!();
        println!(
            "  {} Version list available after install or {}",
            "ℹ".blue(),
                 format!("hpm install {}@<ver>", package).yellow()
        );
    }

    // Install hint
    println!();
    if installed_ver.is_none() {
        println!(
            "  {} Install: {}",
            "→".yellow(),
                 format!("hpm install {}", package).bold().yellow()
        );
    }
    println!();

    Ok(())
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
