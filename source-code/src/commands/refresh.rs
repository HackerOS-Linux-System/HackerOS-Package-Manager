use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use crate::repo::RepoManager;

pub fn refresh() -> Result<()> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}")
        .unwrap(),
    );
    pb.set_message("Downloading package index...");

    let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .into_diagnostic()?;

    let repo_mgr = rt.block_on(RepoManager::load())?;
    let total = repo_mgr.index.packages.len();

    pb.set_message(format!("Pre-fetching metadata for {} packages...", total));

    // Fetch all info.hk files concurrently (empty query = all packages)
    let results = rt.block_on(repo_mgr.search_lightweight(""))?;

    pb.finish_and_clear();

    let ok = results.len();
    let failed = total.saturating_sub(ok);

    println!(
        "{} Package index refreshed — {} packages{}",
        "✔".green(),
             total.to_string().cyan(),
             if failed > 0 {
                 format!(", {} unreachable", failed).yellow().to_string()
             } else {
                 String::new()
             }
    );

    if !results.is_empty() {
        println!();
        println!(
            "  {:<22} {:<12} {}",
            "Package".bold().cyan(),
                 "Version".bold().cyan(),
                 "Description".bold().cyan()
        );
        println!("  {}", "─".repeat(72).dimmed());
        for meta in &results {
            let desc = if meta.summary.len() > 50 {
                format!("{}…", &meta.summary[..49])
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
    }

    Ok(())
}
