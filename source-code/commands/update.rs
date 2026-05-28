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
    let lock   = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let repo_mgr  = RepoManager::load_sync()?;
    let mut state = State::load()?;

    println!("{} Checking for updates...\n", "→".cyan());

    let mut to_update: Vec<(String, String, String)> = Vec::new(); // (name, old, new)

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };

        // Skip pinned
        if let Some(info) = versions.get(&current_ver) {
            if info.pinned {
                println!("  {} {} is pinned at {} — skipping",
                         "⊙".dimmed(), pkg_name.dimmed(), current_ver.dimmed());
                continue;
            }
        }

        let pkg_url = match repo_mgr.get_package_url(pkg_name) {
            Some(url) => url,
            None => {
                println!("  {} {} not found in index — skipping", "⚠".yellow(), pkg_name);
                continue;
            }
        };

        let repo_path = repos_dir().join(pkg_name);
        if repo_path.exists() {
            fetch_repo_incremental(pkg_name, pkg_url)?;
        } else {
            repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
        }

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

    // ── Sprawdź kompatybilność zależności przed aktualizacją ─────────────────
    // Dla każdego pakietu który będzie zaktualizowany, sprawdź czy jego nowe
    // wymagania dep są spełnione przez bieżące lub planowane wersje.
    let dep_issues = check_dependency_compatibility(&to_update, &repo_mgr, &state);
    if !dep_issues.is_empty() {
        println!("{} Dependency compatibility issues detected:\n", "⚠".yellow());
        for issue in &dep_issues {
            println!("  {} {}", "⚠".yellow(), issue);
        }
        println!();
        println!("  These will be resolved automatically during update.");
        println!();
    }

    state.push_snapshot(&format!("pre-update {} packages", to_update.len()));

    let mut updated = 0usize;
    for (pkg_name, old_ver, new_ver) in &to_update {
        println!("{} Updating {} {} → {}",
                 "→".yellow(), pkg_name.cyan(), old_ver.red(), new_ver.green());

        // install_single obsłuży też rekurencyjne aktualizacje zależności
        install_single(pkg_name, Some(new_ver), &repo_mgr, &mut state, true)?;

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

/// Sprawdź czy nowe wersje pakietów będą kompatybilne z ich zależnościami.
/// Zwraca listę opisów problemów (nie bail! — tylko ostrzeżenia, bo install_single je naprawi).
fn check_dependency_compatibility(
    to_update: &[(String, String, String)],
    repo_mgr: &RepoManager,
    state: &State,
) -> Vec<String> {
    let mut issues = Vec::new();

    for (pkg_name, _old_ver, new_ver) in to_update {
        // Pobierz manifest nowej wersji z lokalnego repo (jeśli dostępny)
        let repo_path = repos_dir().join(pkg_name);
        if !repo_path.exists() { continue; }

        let repo = match git2::Repository::open(&repo_path) {
            Ok(r)  => r,
            Err(_) => continue,
        };

        let tags = match repo.tag_names(None) {
            Ok(t)  => t,
            Err(_) => continue,
        };

        // Znajdź tag dla nowej wersji
        let tag = tags.iter().flatten()
            .find(|t| t.trim_start_matches('v') == new_ver.as_str())
            .map(|t| t.to_string());

        let tag = match tag {
            Some(t) => t,
            None    => continue,
        };

        let obj = match repo.revparse_single(&tag) {
            Ok(o)  => o,
            Err(_) => continue,
        };
        let commit = match obj.peel_to_commit() {
            Ok(c)  => c,
            Err(_) => continue,
        };
        let tree = match commit.tree() {
            Ok(t)  => t,
            Err(_) => continue,
        };

        let entry = match tree.get_path(std::path::Path::new("info.hk")) {
            Ok(e)  => e,
            Err(_) => continue,
        };
        let blob = match repo.find_blob(entry.id()) {
            Ok(b)  => b,
            Err(_) => continue,
        };
        let content = match String::from_utf8(blob.content().to_vec()) {
            Ok(c)  => c,
            Err(_) => continue,
        };

        let tmp = match tempfile::tempdir() {
            Ok(t)  => t,
            Err(_) => continue,
        };
        if std::fs::write(tmp.path().join("info.hk"), &content).is_err() { continue; }

        let manifest = match crate::manifest::Manifest::load_from_path(tmp.path().to_str().unwrap()) {
            Ok(m)  => m,
            Err(_) => continue,
        };

        // Sprawdź każdą zależność nowej wersji
        for (dep_name, dep_req) in &manifest.deps {
            let satisfied = state.packages.get(dep_name)
                .map(|vers| vers.keys().any(|v| crate::utils::satisfies(v, dep_req)))
                .unwrap_or(false);

            // Sprawdź też czy dep będzie zaktualizowany do kompatybilnej wersji
            let will_be_updated = to_update.iter()
                .any(|(n, _, nv)| n == dep_name && crate::utils::satisfies(nv, dep_req));

            if !satisfied && !will_be_updated {
                // Sprawdź czy dep jest w repozytorium i ma kompatybilną wersję
                if let Some(versions) = repo_mgr.get_package_versions(dep_name) {
                    let has_compatible = versions.iter()
                        .any(|v| crate::utils::satisfies(v, dep_req));
                    if !has_compatible {
                        issues.push(format!(
                            "{}@{} requires {}{}  but no compatible version exists in repo",
                            pkg_name, new_ver, dep_name,
                            if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) }
                        ));
                    } else {
                        issues.push(format!(
                            "{}@{} requires {}{} — will auto-install compatible version",
                            pkg_name, new_ver, dep_name,
                            if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) }
                        ));
                    }
                }
            }
        }
    }

    issues
}

/// Wykonaj inkrementalny `git fetch` na już sklonowanym repo.
fn fetch_repo_incremental(pkg_name: &str, url: &str) -> Result<()> {
    let repo_path = repos_dir().join(pkg_name);
    let repo = match git2::Repository::open(&repo_path) {
        Ok(r)  => r,
        Err(_) => return Ok(()),
    };

    let mut remote = repo.find_remote("origin").into_diagnostic()?;

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
    fetch_opts.prune(git2::FetchPrune::On);

    remote.fetch(
        &["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"],
        Some(&mut fetch_opts),
        Some("hpm incremental fetch"),
    ).map_err(|e| miette::miette!("git fetch failed for {}: {}", pkg_name, e))?;

    Ok(())
}
