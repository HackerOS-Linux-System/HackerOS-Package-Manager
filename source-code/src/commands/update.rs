use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::path::PathBuf;
use dirs;
use crate::{
    repo::RepoManager,
    state::State,
    commands::install::install_single,
    commands::remove::remove_version,
    utils::compare_versions,
};

fn repos_dir() -> PathBuf {
    dirs::cache_dir()
    .unwrap_or_else(|| PathBuf::from("/tmp"))
    .join("hpm/repos")
}

pub fn update() -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let mut state = State::load()?;

    println!("{} Checking for updates...\n", "→".cyan());

    let mut to_update: Vec<(String, String, String)> = Vec::new(); // (name, old, new)

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None => continue,
        };

        // Skip pinned
        if let Some(info) = versions.get(&current_ver) {
            if info.pinned {
                println!("  {} {} is pinned at {} — skipping",
                         "⊙".dimmed(), pkg_name.dimmed(), current_ver.dimmed());
                continue;
            }
        }

        // Get URL for incremental fetch
        let pkg_url = match repo_mgr.get_package_url(pkg_name) {
            Some(url) => url,
            None => {
                println!("  {} {} not found in index — skipping", "⚠".yellow(), pkg_name);
                continue;
            }
        };

        // Incremental fetch via git fetch (not clone)
        let repo_path = repos_dir().join(pkg_name);
        if repo_path.exists() {
            fetch_repo_incremental(pkg_name, pkg_url)?;
        } else {
            // First time — full clone
            repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
        }

        // Read latest tag from the now-updated repo
        let repo = git2::Repository::open(&repo_path).into_diagnostic()?;
        let tags = repo.tag_names(None).into_diagnostic()?;
        let mut tag_versions: Vec<String> = tags.iter().flatten()
        .map(|t| t.trim_start_matches('v').to_string())
        .collect();
        tag_versions.sort_by(|a, b| compare_versions(a, b));

        if let Some(latest) = tag_versions.last() {
            if compare_versions(latest, &current_ver) == std::cmp::Ordering::Greater {
                to_update.push((pkg_name.clone(), current_ver.clone(), latest.clone()));
            }
        }
    }

    if to_update.is_empty() {
        println!("{} All packages are up to date.", "✔".green());
        return Ok(());
    }

    println!("{} Updates available:\n", "→".yellow());
    for (name, old, new) in &to_update {
        println!("  {} {} {} → {}",
                 "↑".cyan(), name.cyan(), old.red(), new.green());
    }
    println!();

    state.push_snapshot(&format!("pre-update {} packages", to_update.len()));

    let mut updated = 0usize;
    for (pkg_name, old_ver, new_ver) in &to_update {
        println!("{} Updating {} {} → {}",
                 "→".yellow(), pkg_name.cyan(), old_ver.red(), new_ver.green());

        // Install new version
        install_single(pkg_name, Some(new_ver), &repo_mgr, &mut state, true)?;

        // Remove old version (keep files if pinned — already guarded above)
        if let Err(e) = remove_version(pkg_name, old_ver, &mut state) {
            eprintln!("  {} Could not remove old version {}@{}: {}",
                      "⚠".yellow(), pkg_name, old_ver, e);
        }

        updated += 1;
    }

    state.save()?;
    println!("\n{} Updated {} package(s).", "✔".green(), updated);
    Ok(())
}

/// Perform an incremental `git fetch` on an already-cloned repo.
/// Much faster than re-cloning because it only transfers new objects.
fn fetch_repo_incremental(pkg_name: &str, url: &str) -> Result<()> {
    let repo_path = repos_dir().join(pkg_name);
    let repo = match git2::Repository::open(&repo_path) {
        Ok(r) => r,
        Err(_) => return Ok(()), // will be cloned fresh by caller
    };

    let mut remote = repo.find_remote("origin").into_diagnostic()?;

    // Ensure the remote URL is still correct
    if remote.url().unwrap_or("") != url {
        repo.remote_delete("origin").into_diagnostic()?;
        repo.remote("origin", url).into_diagnostic()?;
        remote = repo.find_remote("origin").into_diagnostic()?;
    }

    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.credentials(|url, _, _| {
        if url.starts_with("https://") {
            git2::Cred::userpass_plaintext("", "")
        } else {
            git2::Cred::ssh_key_from_agent("git")
        }
    });

    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);
    fetch_opts.download_tags(git2::AutotagOption::All);
    fetch_opts.prune(git2::FetchPrune::On); // remove deleted remote branches

    remote.fetch(
        &["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"],
        Some(&mut fetch_opts),
                 Some("hpm incremental fetch"),
    ).map_err(|e| miette::miette!("git fetch failed for {}: {}", pkg_name, e))?;

    Ok(())
}
