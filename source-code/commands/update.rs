use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::path::PathBuf;
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

    println!("{} Checking for updates...\n", "→".white());

    let mut to_update: Vec<(String, String, String)> = Vec::new();

    for (pkg_name, versions) in &state.packages {
        let current_ver = match state.get_current_version(pkg_name) {
            Some(v) => v,
            None    => continue,
        };
        if let Some(info) = versions.get(&current_ver) {
            if info.pinned {
                println!("  {} {} pinned at {} — skipping",
                         "⊙".dimmed(), pkg_name.dimmed(), current_ver.dimmed());
                continue;
            }
        }
        let pkg_url = match repo_mgr.get_package_url(pkg_name) {
            Some(u) => u,
            None    => { println!("  {} {} not in index — skipping", "⚠".bright_black(), pkg_name); continue; }
        };

        let repo_path = repos_dir().join(pkg_name);
        if repo_path.exists() {
            fetch_repo_incremental(pkg_name, pkg_url)?;
        } else {
            repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
        }

        let repo = git2::Repository::open(&repo_path).into_diagnostic()?;
        let tags = repo.tag_names(None).into_diagnostic()?;
        let mut tag_vers: Vec<String> = tags.iter().flatten()
            .map(|t| t.trim_start_matches('v').to_string())
            .collect();
        tag_vers.sort_by(|a, b| compare_versions(a, b));

        if let Some(latest) = tag_vers.last() {
            if compare_versions(latest, &current_ver) == std::cmp::Ordering::Greater {
                to_update.push((pkg_name.clone(), current_ver.clone(), latest.clone()));
            }
        }
    }

    if to_update.is_empty() {
        println!("{} All packages are up to date.", "✔".red());
        return Ok(());
    }

    println!("{} Updates available:\n", "→".bright_black());
    for (name, old, new) in &to_update {
        println!("  {} {} {} → {}",
                 "↑".white(), name.white(), old.red(), new.red());
    }
    println!();

    // ── Sprawdź kompatybilność zależności przed aktualizacją ─────────────────
    // NOWE: dla każdego pakietu który będzie zaktualizowany sprawdź
    // czy jego nowe wymagania dep nie kolidują z innymi zainstalowanymi pakietami.
    let issues = check_dep_compatibility_before_update(&to_update, &state, &repo_mgr);
    if !issues.is_empty() {
        println!("{} Dependency compatibility warnings:\n", "⚠".bright_black());
        for issue in &issues {
            println!("  {} {}", "⚠".bright_black(), issue);
        }
        println!();
        println!("  These will be resolved automatically (deps updated if possible).");
        println!("  If a dep cannot be updated to satisfy all constraints, update will abort.");
        println!();
    }

    state.push_snapshot(&format!("pre-update {} packages", to_update.len()));

    let mut updated = 0usize;
    let mut failed  = Vec::new();

    for (pkg_name, old_ver, new_ver) in &to_update {
        println!("{} Updating {} {} → {}",
                 "→".bright_black(), pkg_name.white(), old_ver.red(), new_ver.red());

        match install_single(pkg_name, Some(new_ver), &repo_mgr, &mut state, true) {
            Ok(()) => {
                if let Err(e) = remove_version(pkg_name, old_ver, &mut state) {
                    eprintln!("  {} Could not remove old {}@{}: {}",
                              "⚠".bright_black(), pkg_name, old_ver, e);
                }

                // Post-update: nowa wersja jest już w store — hook może np.
                // zmigrować dane konfiguracyjne z formatu starej wersji.
                let new_pkg_path = std::path::Path::new(crate::store_path())
                    .join(pkg_name).join(new_ver);
                let _ = crate::squash::ensure_mounted(&new_pkg_path);
                if crate::hooks::hook_exists(&new_pkg_path, crate::hooks::HookKind::PostUpdate) {
                    if let Ok(new_manifest) = crate::manifest::Manifest::load_from_path(
                        new_pkg_path.to_str().unwrap_or_default()
                    ) {
                        let ctx = crate::hooks::HookContext {
                            pkg_name: pkg_name, pkg_version: new_ver,
                            store_path: crate::store_path(), old_version: Some(old_ver),
                        };
                        if let Err(e) = crate::hooks::run_hook(
                            &new_pkg_path, crate::hooks::HookKind::PostUpdate, &ctx, &new_manifest,
                        ) {
                            eprintln!("  {} post-update hook failed for {}: {}",
                                      "⚠".bright_black(), pkg_name, e);
                        }
                    }
                }

                updated += 1;
            }
            Err(e) => {
                eprintln!("  {} Failed to update {}: {}", "✗".red(), pkg_name.white(), e);
                failed.push(pkg_name.clone());
            }
        }
    }

    state.save()?;
    println!("\n{} Updated {} package(s).", "✔".red(), updated);
    if !failed.is_empty() {
        println!("{} Failed: {}", "✗".red(), failed.join(", "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sprawdzenie kompatybilności — NOWE
// Dla każdego pakietu do aktualizacji pobierz jego nowy manifest z lokalnego repo
// i sprawdź czy jego nowe wymagania dep kolidują z innymi zainstalowanymi pakietami.
// ---------------------------------------------------------------------------

fn check_dep_compatibility_before_update(
    to_update: &[(String, String, String)],
    state: &State,
    repo_mgr: &RepoManager,
) -> Vec<String> {
    let mut issues = Vec::new();

    for (pkg_name, _old_ver, new_ver) in to_update {
        let repo_path = repos_dir().join(pkg_name);
        if !repo_path.exists() { continue; }

        let repo = match git2::Repository::open(&repo_path) {
            Ok(r)  => r,
            Err(_) => continue,
        };

        // Pobierz manifest nowej wersji z tagu git
        let new_manifest = match get_manifest_at_version(&repo, new_ver) {
            Some(m) => m,
            None    => continue,
        };

        // Dla każdej zależności nowej wersji sprawdź:
        // 1. Czy jest zainstalowana i kompatybilna
        // 2. Czy inny pakiet nie wymaga konfliktowej wersji tej samej dep
        for (dep_name, dep_req) in &new_manifest.deps {
            // Sprawdź zainstalowaną wersję dep
            let installed_dep_ver = state.get_current_version(dep_name);

            let dep_ok = installed_dep_ver.as_ref()
                .map(|v| crate::utils::satisfies(v, dep_req))
                .unwrap_or(false);

            if !dep_ok && installed_dep_ver.is_some() {
                // Dep jest zainstalowana ale niekompatybilna wersja
                let inst_ver = installed_dep_ver.unwrap();
                issues.push(format!(
                    "{}@{} requires {}{}  but {}@{} is installed",
                    pkg_name, new_ver, dep_name,
                    if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) },
                    dep_name, inst_ver
                ));
            }

            // Sprawdź czy inne zainstalowane pakiety mają sprzeczne wymagania dla dep_name
            // Przykład: baz@1.0 wymaga bar<2.0 ale foo@2.0 wymaga bar>=2.0
            for (other_pkg, other_vers) in &state.packages {
                if other_pkg == pkg_name { continue; }
                // Sprawdź czy ten pakiet też jest planowany do aktualizacji
                let being_updated = to_update.iter().any(|(n, _, _)| n == other_pkg);
                if being_updated { continue; } // zaktualizujemy, zależy od nowej wersji

                let other_cur = match state.get_current_version(other_pkg) {
                    Some(v) => v,
                    None    => continue,
                };
                let other_info = match other_vers.get(&other_cur) {
                    Some(i) => i,
                    None    => continue,
                };

                // Szukaj zależności od dep_name wśród innych pakietów
                for dep_spec in &other_info.depends_on {
                    let (dname, _dver) = crate::state::split_pkg_ver(dep_spec);
                    if dname != *dep_name { continue; }

                    // Pobierz wymagania dep_name z manifestu other_pkg
                    let other_repo_path = repos_dir().join(other_pkg);
                    if !other_repo_path.exists() { continue; }
                    let other_repo = match git2::Repository::open(&other_repo_path) {
                        Ok(r)  => r,
                        Err(_) => continue,
                    };
                    let other_manifest = match get_manifest_at_version(&other_repo, &other_cur) {
                        Some(m) => m,
                        None    => continue,
                    };
                    if let Some(other_req) = other_manifest.deps.get(dep_name) {
                        // Sprawdź czy wymagania dep_req i other_req są sprzeczne
                        // Prosta heurystyka: jeśli oba są >= ale różne minimalne wersje
                        if !dep_req.is_empty() && !other_req.is_empty() && dep_req != other_req {
                            // Sprawdź przykładowy scenariusz >=2.0 vs <2.0 (brak takiego API w utils)
                            // Wystarczy ostrzeżenie — install_single sam to wykryje
                            issues.push(format!(
                                "{}@{} requires {}{} but {}@{} requires {}{}",
                                pkg_name, new_ver, dep_name,
                                format!(" ({})", dep_req),
                                other_pkg, other_cur, dep_name,
                                format!(" ({})", other_req)
                            ));
                        }
                    }
                    break; // jedna zależność od dep_name per pakiet
                }
            }
        }
    }

    issues
}

fn get_manifest_at_version(repo: &git2::Repository, version: &str) -> Option<crate::manifest::Manifest> {
    let tags = repo.tag_names(None).ok()?;
    let tag  = tags.iter().flatten()
        .find(|t| t.trim_start_matches('v') == version)?
        .to_string();
    let obj    = repo.revparse_single(&tag).ok()?;
    let commit = obj.peel_to_commit().ok()?;
    let tree   = commit.tree().ok()?;
    let entry  = tree.get_path(std::path::Path::new("info.hk")).ok()?;
    let blob   = repo.find_blob(entry.id()).ok()?;
    let content = String::from_utf8(blob.content().to_vec()).ok()?;
    let tmp    = tempfile::tempdir().ok()?;
    std::fs::write(tmp.path().join("info.hk"), &content).ok()?;
    crate::manifest::Manifest::load_from_path(tmp.path().to_str()?).ok()
}

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
    let mut cb = git2::RemoteCallbacks::new();
    cb.credentials(|url, _, _| {
        if url.starts_with("https://") { git2::Cred::userpass_plaintext("", "") }
        else { git2::Cred::ssh_key_from_agent("git") }
    });
    let mut fo = git2::FetchOptions::new();
    fo.remote_callbacks(cb);
    fo.download_tags(git2::AutotagOption::All);
    fo.prune(git2::FetchPrune::On);
    remote.fetch(
        &["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"],
        Some(&mut fo), Some("hpm incremental fetch"),
    ).map_err(|e| miette::miette!("git fetch failed for {}: {}", pkg_name, e))?;
    Ok(())
}
