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

    pb.set_message(format!("Fetching metadata for {} packages...", total));

    // Pre-fetch all info.hk concurrently so search/info are fast afterwards
    let results = rt.block_on(repo_mgr.search_lightweight(""))?;

    pb.finish_and_clear();

    let ok = results.len();
    let failed = total.saturating_sub(ok);

    // ── apt-like output ──────────────────────────────────────────────────────
    println!("{} Get package index from HackerOS Package Repository", "→".blue());
    println!(
        "{} Reading package lists... {}",
        "→".blue(),
             "Done".green()
    );
    println!(
        "{} Building dependency tree... {}",
        "→".blue(),
             "Done".green()
    );
    println!(
        "{} Reading state information... {}",
        "→".blue(),
             "Done".green()
    );

    if failed > 0 {
        println!(
            "{} {} package(s) could not be reached.",
                 "⚠".yellow(), failed
        );
    }

    println!(
        "\n{} packages available ({} newly fetched).",
             total.to_string().cyan(),
             ok.to_string().green()
    );

    Ok(())
}
