use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use crate::repo::RepoManager;

pub fn search(query: String) -> Result<()> {
    if query.is_empty() {
        eprintln!("{} Usage: hpm search <query>", "✗".red());
        std::process::exit(1);
    }

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

    pb.set_message(format!(
        "Searching {} packages...",
        repo_mgr.index.packages.len()
    ));

    let results = rt.block_on(repo_mgr.search_lightweight(&query))?;

    pb.finish_and_clear();

    if results.is_empty() {
        println!("{} No results found for '{}'.", "✗".red(), query.cyan());
        println!(
            "  Tip: try a different keyword, or run {} to refresh.",
            "hpm refresh".yellow()
        );
        return Ok(());
    }

    println!(
        "{} Search results for '{}' ({} found):\n",
             "→".blue(), query.cyan(), results.len()
    );

    println!(
        "  {:<22} {:<12} {}",
        "Package".bold().cyan(),
             "Version".bold().cyan(),
             "Description".bold().cyan()
    );
    println!("  {}", "─".repeat(72).dimmed());

    for meta in &results {
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
    }

    println!();
    println!(
        "  Run {} for details, {} to install.",
        "hpm info <package>".yellow(),
             "hpm install <package>".yellow()
    );

    Ok(())
}
