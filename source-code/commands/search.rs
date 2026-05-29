use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use crate::repo::RepoManager;

pub fn search(query: String) -> Result<()> {
    if query.is_empty() {
        eprintln!("{} Usage: hpm search <query|@tag>", "✗".red());
        std::process::exit(1);
    }

    // Wykryj czy to wyszukiwanie po tagu grupowym
    let is_tag_search = query.starts_with('@');

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Loading package index...");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .into_diagnostic()?;

    let repo_mgr = rt.block_on(RepoManager::load())?;

    if is_tag_search {
        // Tryb wyszukiwania po tagu — pokaż od razu z indeksu bez HTTP
        pb.finish_and_clear();
        let tag  = query.trim_start_matches('@');
        let pkgs = repo_mgr.packages_for_tag(tag);

        if pkgs.is_empty() {
            println!("{} No packages found for tag '{}'.", "✗".red(), query.cyan());
            let all_tags = repo_mgr.all_tags();
            if !all_tags.is_empty() {
                println!("  Available tags: {}",
                    all_tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", "));
            }
            println!("  Tip: run {} to see all tags.", "hpm tags".yellow());
            return Ok(());
        }

        println!(
            "{} Packages with tag '{}' ({} found):\n",
            "→".blue(), query.cyan(), pkgs.len()
        );
        println!("  {:<22} {:<12} {}", "Package".bold().cyan(), "Version".bold().cyan(), "Description".bold().cyan());
        println!("  {}", "─".repeat(72).dimmed());

        // Pobierz metadane dla pakietów z tagu
        pb.set_message(format!("Fetching metadata for {} packages...", pkgs.len()));

        let tag_query   = tag.to_string();
        let tag_results = rt.block_on(repo_mgr.search_lightweight(&format!("@{}", tag_query)))?;

        // Pokaż wyniki — filtruj tylko te z tego tagu
        let mut displayed = 0;
        for meta in &tag_results {
            if !pkgs.contains(&meta.name) { continue; }
            let desc = if meta.summary.len() > 52 {
                format!("{}…", &meta.summary[..51])
            } else {
                meta.summary.clone()
            };
            println!(
                "  {:<22} {:<12} {}",
                meta.name.magenta(),
                meta.version.green(),
                desc
            );
            displayed += 1;
        }

        // Jeśli metadane nie były w cache — pokaż przynajmniej nazwy
        if displayed < pkgs.len() {
            for pkg_name in &pkgs {
                if tag_results.iter().any(|m| &m.name == pkg_name) { continue; }
                println!(
                    "  {:<22} {:<12} {}",
                    pkg_name.magenta(),
                    "unknown".dimmed(),
                    "(run hpm refresh to fetch metadata)".dimmed()
                );
            }
        }

        println!();
        println!(
            "  Install all: {}",
            format!("hpm install @{}", tag).yellow()
        );
        println!(
            "  Run {} for details, {} to install.",
            "hpm info <package>".yellow(),
            "hpm install <package>".yellow()
        );
        return Ok(());
    }

    // Normalny tryb wyszukiwania po nazwie/opisie
    pb.set_message(format!("Searching {} packages...", repo_mgr.index.packages.len()));
    let results = rt.block_on(repo_mgr.search_lightweight(&query))?;
    pb.finish_and_clear();

    if results.is_empty() {
        println!("{} No results found for '{}'.", "✗".red(), query.cyan());
        println!("  Tip: try a different keyword, or run {} to refresh.", "hpm refresh".yellow());
        println!("  To search by group tag: {}", format!("hpm search @<tag>", ).yellow());
        return Ok(());
    }

    println!(
        "{} Search results for '{}' ({} found):\n",
        "→".blue(), query.cyan(), results.len()
    );
    println!(
        "  {:<22} {:<12} {:<30} {}",
        "Package".bold().cyan(),
        "Version".bold().cyan(),
        "Description".bold().cyan(),
        "Tags".bold().cyan()
    );
    println!("  {}", "─".repeat(80).dimmed());

    for meta in &results {
        let desc = if meta.summary.len() > 30 {
            format!("{}…", &meta.summary[..29])
        } else {
            meta.summary.clone()
        };
        let tags_str = if meta.tags.is_empty() {
            String::new()
        } else {
            meta.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(" ")
        };
        println!(
            "  {:<22} {:<12} {:<30} {}",
            meta.name.magenta(),
            meta.version.green(),
            desc,
            tags_str.dimmed()
        );
    }

    println!();
    println!(
        "  Run {} for details, {} to install.",
        "hpm info <package>".yellow(),
        "hpm install <package>".yellow()
    );
    println!(
        "  To search by tag: {}",
        "hpm search @<tag>".yellow()
    );
    Ok(())
}
