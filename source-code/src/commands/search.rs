use anyhow::Result;
use colored::Colorize;
use crate::repo::RepoManager;

pub fn search(query: String) -> Result<()> {
    let repo_mgr = RepoManager::load_sync()?;
    let index = repo_mgr.build_index()?;

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for (name, pkg) in &index {
        let name_lower = name.to_lowercase();
        let desc_lower = pkg.versions.last()
        .map(|v| v.manifest.summary.to_lowercase())
        .unwrap_or_default();

        if name_lower.contains(&query_lower) || desc_lower.contains(&query_lower) {
            let latest_ver = pkg.versions.last()
            .map(|v| v.version.clone())
            .unwrap_or_else(|| "unknown".to_string());
            let short_desc = pkg.versions.last()
            .map(|v| {
                if v.manifest.summary.len() > 50 {
                    format!("{}...", &v.manifest.summary[..47])
                } else {
                    v.manifest.summary.clone()
                }
            })
            .unwrap_or_default();
            results.push((name.clone(), latest_ver, short_desc));
        }
    }

    if results.is_empty() {
        println!("{} No results found for '{}'.", "✗".red(), query);
        return Ok(());
    }

    println!("{} Search results for '{}':", "→".blue(), query);
    println!("{:<20} {:<15} {}", "Package".cyan(), "Version".cyan(), "Description".cyan());
    for (name, ver, desc) in results {
        println!("{:<20} {:<15} {}", name.magenta(), ver.green(), desc);
    }

    Ok(())
}
