use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use crate::{
    repo::RepoManager,
    state::State,
    utils::compare_versions,
};

pub fn outdated() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let index    = repo_mgr.build_index()?;
    let state    = State::load()?;

    let mut outdated_list = Vec::new();

    for (pkg_name, _) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        // Nie pokazuj pinowanych
        if let Some(info) = state.packages[pkg_name].get(&current_ver) {
            if info.pinned {
                continue;
            }
        }
        let repo_pkg = match index.get(pkg_name) {
            Some(p) => p,
            None    => continue,
        };
        if let Some(latest) = repo_pkg.versions.iter()
            .map(|v| &v.version)
            .max_by(|a, b| compare_versions(a, b))
        {
            if compare_versions(latest, &current_ver) == std::cmp::Ordering::Greater {
                outdated_list.push((pkg_name.clone(), current_ver, latest.clone()));
            }
        }
    }

    if outdated_list.is_empty() {
        println!("{} All packages are up to date.", "✔".green());
    } else {
        println!("{} Outdated packages:\n", "→".yellow());
        println!("  {:<20} {:<15} {}", "Package".cyan(), "Current".cyan(), "Latest".cyan());
        println!("  {}", "─".repeat(55).dimmed());
        for (pkg, cur, lat) in &outdated_list {
            println!("  {:<20} {:<15} {}", pkg.magenta(), cur.red(), lat.green());
        }
        println!();
        println!("  Run {} to update all.", "hpm update".yellow());
    }
    Ok(())
}
