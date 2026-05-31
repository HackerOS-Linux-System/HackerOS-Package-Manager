use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::path::PathBuf;
use crate::{repo::RepoManager, state::State};

pub fn info(package: String) -> Result<()> {
    if package.is_empty() {
        eprintln!("{} Usage: hpm info <package>", "✗".red());
        std::process::exit(1);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;

    let repo_mgr = rt.block_on(RepoManager::load())?;
    let state    = State::load()?;

    // Sprawdź czy pakiet jest w indeksie
    let repo_url = repo_mgr.get_package_url(&package)
        .ok_or_else(|| miette::miette!(
            "Package '{}' not found in repository index.\n  Run {} to refresh.",
            package, "hpm refresh".yellow()
        ))?
        .to_string();

    // Pobierz metadane z info.hk (cache lub HTTP)
    let meta      = rt.block_on(repo_mgr.fetch_package_meta(&package))?;
    let build_cfg = rt.block_on(repo_mgr.fetch_raw_build_config(&repo_url));

    let installed_ver = state.get_current_version(&package);
    let pinned = installed_ver.as_ref()
        .and_then(|ver| state.packages.get(&package)?.get(ver))
        .map(|i| i.pinned)
        .unwrap_or(false);

    // Wersje z lokalnie sklonowanego repo (tagi git)
    // ŻADNE wersje nie pochodzą z repo.json
    let local_versions = crate::repo::load_cached_meta_pub(&package)
        .map(|m| m.available_versions)
        .unwrap_or_default();

    // ── Wydruk ───────────────────────────────────────────────────────────────
    println!();
    println!("  {} {}", "◆".cyan(), package.bold().cyan());
    println!("  {}", "─".repeat(60).dimmed());
    println!("  {:<16} {}", "Version:".bold(),    meta.version.green());
    println!("  {:<16} {}", "Author:".bold(),     meta.authors);
    println!("  {:<16} {}", "License:".bold(),    meta.license);
    println!("  {:<16} {}", "Repository:".bold(), repo_url.dimmed());

    // Tagi (z info.hk, nie z repo.json)
    if !meta.tags.is_empty() {
        let tags_str = meta.tags.iter()
            .map(|t| format!("@{}", t).cyan().to_string())
            .collect::<Vec<_>>().join("  ");
        println!("  {:<16} {}", "Tags:".bold(), tags_str);
    }

    // Build type z build.toml
    if let Some(ref cfg) = build_cfg {
        let build_type = match &cfg.source {
            crate::repo::BuildSource::Download { url, .. } => {
                let trimmed = if url.len() > 50 { format!("{}…", &url[..49]) } else { url.clone() };
                format!("download ({})", trimmed.dimmed())
            }
            crate::repo::BuildSource::Build { .. } => "build from source".to_string(),
            crate::repo::BuildSource::Prebuilt    => "prebuilt (contents/)".to_string(),
        };
        println!("  {:<16} {}", "Build type:".bold(), build_type);
    }

    // Opis
    println!();
    println!("  {}", "Description:".bold());
    for line in wrap_text(&meta.summary, 65) {
        println!("    {}", line);
    }

    // Status instalacji
    println!();
    if let Some(ref ver) = installed_ver {
        let pin_tag = if pinned { format!(" {}", "(pinned)".yellow()) } else { String::new() };
        println!("  {:<16} {}{}", "Installed:".bold(), ver.cyan(), pin_tag);

        // Pokaż niestandardowe nazwy wrapperów
        let wn = crate::state::WrapperNames::load();
        let store_path = std::path::Path::new(crate::STORE_PATH).join(&package).join(ver);
        if let Ok(manifest) = crate::manifest::Manifest::load_from_path(store_path.to_str().unwrap_or("")) {
            let custom: Vec<_> = manifest.bins.iter()
                .filter_map(|b| wn.get(&package, b).filter(|&w| w != b).map(|w| format!("{} → /usr/bin/{}", b, w)))
                .collect();
            if !custom.is_empty() {
                println!("  {:<16} {}", "Wrappers:".bold(), custom.join(", ").dimmed());
            }
        }
    } else {
        println!("  {:<16} {}", "Installed:".bold(), "No".red());
    }

    // Wersje z tagów git (sklonowane lokalnie przez hpm install lub hpm refresh)
    if !local_versions.is_empty() {
        println!();
        println!("  {}", "Available versions (from git tags):".bold());
        for v in &local_versions {
            let cur = if installed_ver.as_deref() == Some(v.as_str()) {
                format!(" {}", "← current".green())
            } else { String::new() };
            println!("    • {}{}", v.cyan(), cur);
        }
    } else {
        println!();
        println!("  {} Run {} or {} to see all versions.",
                 "ℹ".blue(),
                 "hpm refresh".yellow(),
                 format!("hpm install {}@<ver>", package).yellow());
    }

    // Powiązane pakiety z tych samych tagów
    if !meta.tags.is_empty() {
        let mut any_related = false;
        for tag in &meta.tags {
            let related: Vec<_> = repo_mgr.packages_for_tag(tag).into_iter()
                .filter(|p| p != &package).take(4).collect();
            if !related.is_empty() {
                if !any_related {
                    println!();
                    println!("  {} Related packages (same tags):", "ℹ".blue());
                    any_related = true;
                }
                println!("    @{}: {}", tag.cyan(),
                    related.iter().map(|p| p.magenta().to_string()).collect::<Vec<_>>().join(", "));
            }
        }
    }

    // Hint instalacji
    println!();
    if installed_ver.is_none() {
        println!("  {} Install: {}", "→".yellow(),
                 format!("hpm install {}", package).bold().yellow());
    }
    println!();
    Ok(())
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines   = Vec::new();
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
    if !current.is_empty() { lines.push(current); }
    if lines.is_empty()    { lines.push(String::new()); }
    lines
}
