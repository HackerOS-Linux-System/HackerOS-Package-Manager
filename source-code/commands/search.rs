use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{self, Write};
use crate::repo::RepoManager;

const PAGE_SIZE: usize = 20;

pub fn search(query: String) -> Result<()> {
    if query.is_empty() {
        eprintln!("{} Usage: hpm search <query|@tag>", "✗".red());
        eprintln!("  {}  search by tag group", "hpm search @development".bright_black());
        eprintln!("  {}  search by name/description", "hpm search editor".bright_black());
        std::process::exit(1);
    }

    let is_tag = query.starts_with('@');

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}").unwrap());
    pb.set_message("Loading package index...");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;

    if is_tag {
        return search_by_tag(&query, &repo_mgr, &rt);
    }

    pb.set_message(format!("Searching {} packages...", repo_mgr.index.packages.len()));
    let results = rt.block_on(repo_mgr.search_lightweight(&query))?;
    pb.finish_and_clear();

    if results.is_empty() {
        println!("{} No results for '{}'.", "✗".red(), query.white());
        println!("  Try a different keyword or run {} to refresh.", "hpm refresh".bright_black());
        println!("  Search by tag: {}", "hpm search @<tag>".bright_black());
        return Ok(());
    }

    let total = results.len();

    println!(
        "{} Results for '{}' ({} found):\n",
        "→".white(), query.white(), total
    );

    // Paginacja
    if total <= PAGE_SIZE {
        print_results_table(&results, 0, total);
        print_hints();
    } else {
        // Interaktywna paginacja
        let mut page = 0usize;
        let total_pages = (total + PAGE_SIZE - 1) / PAGE_SIZE;

        loop {
            let start = page * PAGE_SIZE;
            let end   = (start + PAGE_SIZE).min(total);

            println!("{}", format!("Page {}/{} ({}-{} of {})",
                page + 1, total_pages, start + 1, end, total).dimmed());
            println!();

            print_results_table(&results, start, end);

            println!();
            println!("  {}", format!("Page {}/{}", page + 1, total_pages).bold());

            let mut prompt = String::new();
            if page + 1 < total_pages { prompt.push_str(" [n]ext"); }
            if page > 0               { prompt.push_str(" [p]rev"); }
            prompt.push_str(" [q]uit");

            print!("  {}: ", prompt.dimmed());
            io::stdout().flush().into_diagnostic()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input).into_diagnostic()?;

            match input.trim().to_lowercase().as_str() {
                "n" | "next" | "" => {
                    if page + 1 < total_pages {
                        page += 1;
                        println!();
                    } else {
                        break;
                    }
                }
                "p" | "prev" => {
                    if page > 0 {
                        page -= 1;
                        println!();
                    }
                }
                "q" | "quit" | "exit" => break,
                s => {
                    // Wpisano numer strony
                    if let Ok(n) = s.parse::<usize>() {
                        if n >= 1 && n <= total_pages {
                            page = n - 1;
                            println!();
                            continue;
                        }
                    }
                    break;
                }
            }
        }

        print_hints();
    }

    Ok(())
}

fn search_by_tag(
    query: &str,
    repo_mgr: &RepoManager,
    rt: &tokio::runtime::Runtime,
) -> Result<()> {
    let tag  = query.trim_start_matches('@');
    let pkgs = repo_mgr.packages_for_tag(tag);

    if pkgs.is_empty() {
        println!("{} No packages found for tag '{}'.", "✗".red(), query.white());
        let all = repo_mgr.all_tags();
        if !all.is_empty() {
            println!("  Available tags: {}",
                all.iter().map(|t| format!("@{}", t).white().to_string()).collect::<Vec<_>>().join("  "));
        }
        println!("  List all tags: {}", "hpm tags".bright_black());
        return Ok(());
    }

    let total = pkgs.len();
    println!("{} Packages with tag '{}' ({} total):\n", "→".white(), query.white(), total);

    // Pobierz metadane dla pakietów z tego tagu
    let tag_results = rt.block_on(repo_mgr.search_lightweight(query))?;

    // Mapuj name → meta
    let meta_map: std::collections::HashMap<&str, _> = tag_results.iter()
        .map(|m| (m.name.as_str(), m))
        .collect();

    // Paginacja
    let total_pages = (total + PAGE_SIZE - 1) / PAGE_SIZE;
    let mut page = 0usize;

    loop {
        let start = page * PAGE_SIZE;
        let end   = (start + PAGE_SIZE).min(total);
        let page_pkgs = &pkgs[start..end];

        if total_pages > 1 {
            println!("{}", format!("Page {}/{}", page + 1, total_pages).dimmed());
            println!();
        }

        println!("  {:<22} {:<12} {}", "Package".bold().white(), "Version".bold().white(), "Description".bold().white());
        println!("  {}", "─".repeat(72).dimmed());

        for pkg_name in page_pkgs {
            let (version, desc, tags_str) = if let Some(meta) = meta_map.get(pkg_name.as_str()) {
                let d = if meta.summary.len() > 45 {
                    format!("{}…", &meta.summary[..44])
                } else {
                    meta.summary.clone()
                };
                let t = if meta.tags.is_empty() { String::new() }
                    else { meta.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(" ") };
                (meta.version.clone(), d, t)
            } else {
                ("unknown".to_string(), "(run hpm refresh)".to_string(), String::new())
            };

            println!("  {:<22} {:<12} {} {}",
                pkg_name.magenta(),
                version.red(),
                desc,
                tags_str.dimmed()
            );
        }

        if total_pages <= 1 { break; }

        println!();
        let mut prompt = String::new();
        if page + 1 < total_pages { prompt.push_str(" [n]ext"); }
        if page > 0               { prompt.push_str(" [p]rev"); }
        prompt.push_str(" [q]uit");

        print!("  {}: ", prompt.dimmed());
        io::stdout().flush().into_diagnostic()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).into_diagnostic()?;

        match input.trim().to_lowercase().as_str() {
            "n" | ""   => { if page + 1 < total_pages { page += 1; println!(); } else { break; } }
            "p"        => { if page > 0 { page -= 1; println!(); } }
            "q" | "quit" => break,
            _ => break,
        }
    }

    println!();
    println!("  Install all: {}", format!("hpm install @{}", tag).bright_black());
    println!("  Run {} for details, {} to install single package.",
             "hpm info <package>".bright_black(), "hpm install <package>".bright_black());
    Ok(())
}

fn print_results_table(
    results: &[crate::repo::PackageMeta],
    start: usize,
    end: usize,
) {
    println!("  {:<22} {:<12} {:<32} {}",
        "Package".bold().white(),
        "Version".bold().white(),
        "Description".bold().white(),
        "Tags".bold().white()
    );
    println!("  {}", "─".repeat(80).dimmed());

    for meta in &results[start..end] {
        let desc = if meta.summary.len() > 30 {
            format!("{}…", &meta.summary[..29])
        } else {
            meta.summary.clone()
        };
        let tags_str = if meta.tags.is_empty() { String::new() }
            else {
                meta.tags.iter()
                    .map(|t| format!("@{}", t).dimmed().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
        println!("  {:<22} {:<12} {:<32} {}",
            meta.name.magenta(),
            meta.version.red(),
            desc,
            tags_str
        );
    }
}

fn print_hints() {
    println!();
    println!("  Run {} for details, {} to install.",
             "hpm info <package>".bright_black(),
             "hpm install <package>".bright_black());
    println!("  Search by tag: {}", "hpm search @<tag>".bright_black());
}
