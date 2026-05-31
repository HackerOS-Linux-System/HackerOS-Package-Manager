use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use crate::repo::{RepoManager, invalidate_meta_cache};

pub fn refresh() -> Result<()> {
    // Unieważnij stary cache — pobierzemy świeże dane
    invalidate_meta_cache();

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}").unwrap());
    pb.set_message("Downloading package index...");

    let rt       = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let total    = repo_mgr.index.len();

    pb.set_message(format!(
        "Fetching metadata for {} packages (reading info.hk from each repo)...", total
    ));

    // Pobierz info.hk ze wszystkich repozytoriów równolegle (max 20 naraz)
    let results = rt.block_on(repo_mgr.search_lightweight(""))?;
    pb.finish_and_clear();

    let ok     = results.len();
    let failed = total.saturating_sub(ok);

    println!("{} Get package index from HackerOS Package Repository", "→".blue());
    println!("{} Reading package lists...   {}", "→".blue(), "Done".green());
    println!("{} Fetching metadata...        {} / {} packages", "→".blue(),
             ok.to_string().green(), total.to_string().cyan());

    if failed > 0 {
        println!("{} {} package(s) could not be reached (network error or no info.hk)",
                 "⚠".yellow(), failed);
    }

    // Pokaż tagi grupowe zebrane z info.hk
    let tags = repo_mgr.all_tags();
    if !tags.is_empty() {
        println!("{} Group tags (from info.hk): {}",
            "→".blue(),
            tags.iter().map(|t| format!("@{}", t).cyan().to_string())
                .collect::<Vec<_>>().join("  "));
    }

    println!();
    println!("{} packages available.", total.to_string().cyan());
    println!("  Versions and tags are read from each package's {} — not from repo.json.",
             "info.hk".yellow());
    println!();
    Ok(())
}
