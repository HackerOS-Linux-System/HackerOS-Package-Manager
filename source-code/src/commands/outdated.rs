use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use crate::{
    repo::RepoManager,
    state::State,
    utils::compare_versions,
};

pub fn outdated() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let index = repo_mgr.build_index()?;
    let state = State::load()?;

    let mut outdated = Vec::new();

    for (pkg_name, _) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None => continue,
        };

        let repo_pkg = match index.get(pkg_name) {
            Some(p) => p,
            None => continue,
        };

        let latest_ver = repo_pkg.versions.iter()
        .map(|v| &v.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap();

        if compare_versions(latest_ver, &current_ver) == std::cmp::Ordering::Greater {
            outdated.push((pkg_name.clone(), current_ver, latest_ver.clone()));
        }
    }

    if outdated.is_empty() {
        println!("{} All packages are up to date.", "✔".green());
    } else {
        println!("{} Outdated packages:", "→".yellow());
        println!("{:<20} {:<15} {}", "Package".cyan(), "Current".cyan(), "Latest".cyan());
        for (pkg, cur, lat) in outdated {
            println!("{:<20} {:<15} {}", pkg.as_str().magenta(), cur.as_str().red(), lat.as_str().green());
        }
    }

    Ok(())
}
