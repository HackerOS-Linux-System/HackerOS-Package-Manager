use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use crate::{
    repo::RepoManager,
    state::State,
    commands::install::install_single,
    commands::remove::remove_version,
    utils::compare_versions,
};

pub fn update() -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let index = repo_mgr.build_index()?;
    let mut state = State::load()?;

    let mut updated = 0;
    let mut current = 0;

    for (pkg_name, versions) in state.packages.clone() {
        let current_ver = match state.get_current_version(&pkg_name) {
            Some(v) => v,
            None => continue,
        };

        if let Some(info) = versions.get(&current_ver) {
            if info.pinned {
                current += 1;
                continue;
            }
        }

        let repo_pkg = match index.get(&pkg_name) {
            Some(p) => p,
            None => continue,
        };

        let latest_ver = repo_pkg.versions.iter()
        .map(|v| &v.version)
        .max_by(|a, b| compare_versions(a, b))
        .ok_or_else(|| miette::miette!("No versions for {}", pkg_name))?;

        if compare_versions(latest_ver, &current_ver) == std::cmp::Ordering::Greater {
            println!("{} Updating {} from {} to {}", "→".yellow(), pkg_name.cyan(), current_ver.cyan(), latest_ver.cyan());
            remove_version(&pkg_name, &current_ver, &mut state)?;
            // Instaluj nową wersję – podajemy ją jawnie
            install_single(&pkg_name, Some(latest_ver), &repo_mgr, &mut state)?;
            updated += 1;
        } else {
            current += 1;
        }
    }

    println!("{} Update complete. Updated: {}, Already current: {}", "✔".green(), updated, current);
    Ok(())
}
