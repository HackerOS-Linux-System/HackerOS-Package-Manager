use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use crate::repo::{RepoManager, invalidate_meta_cache};

pub fn refresh() -> Result<()> {
    // Unieważnij cache metadanych żeby wymusić ponowne pobieranie
    invalidate_meta_cache();

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message("Downloading package index...");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let total    = repo_mgr.index.packages.len();

    pb.set_message(format!("Fetching metadata for {} packages...", total));

    // Pre-fetch wszystkich info.hk równolegle (max 20 naraz)
    let results = rt.block_on(repo_mgr.search_lightweight(""))?;
    pb.finish_and_clear();

    let ok     = results.len();
    let failed = total.saturating_sub(ok);

    println!("{} Get package index from HackerOS Package Repository", "→".blue());
    println!("{} Reading package lists... {}", "→".blue(), "Done".green());
    println!("{} Building dependency tree... {}", "→".blue(), "Done".green());
    println!("{} Reading state information... {}", "→".blue(), "Done".green());

    // Pokaż dostępne tagi grupowe
    let tags = repo_mgr.all_tags();
    if !tags.is_empty() {
        println!("{} Group tags: {}",
            "→".blue(),
            tags.iter().map(|t| format!("@{}", t).cyan().to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    if failed > 0 {
        println!("{} {} package(s) could not be reached.", "⚠".yellow(), failed);
    }
    println!(
        "\n{} packages available ({} newly fetched, {} tags).",
        total.to_string().cyan(),
        ok.to_string().green(),
        tags.len().to_string().cyan()
    );
    Ok(())
}
