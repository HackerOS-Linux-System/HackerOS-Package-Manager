use anyhow::{Context, Result};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};
use crate::{
    STORE_PATH,
    manifest::Manifest,
    repo::RepoManager,
    sandbox::setup_sandbox,
    state::State,
    utils::{acquire_lock, release_lock, compute_dir_hash, copy_dir_all, make_executable},
};

pub fn install(specs: Vec<String>) -> Result<()> {
    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let index = repo_mgr.build_index()?;
    let mut state = State::load()?;

    for spec in specs {
        let (pkg_name, requested_ver) = if spec.contains('@') {
            let mut parts = spec.split('@');
            (parts.next().unwrap().to_string(), parts.next().map(String::from))
        } else {
            (spec, None)
        };

        let pkg = index.get(&pkg_name)
        .with_context(|| format!("Package {} not found in repository", pkg_name))?;

        let version = if let Some(v) = requested_ver {
            v
        } else {
            pkg.versions.last().context("No versions available")?.version.clone()
        };

        if let Some(vers) = state.packages.get(&pkg_name) {
            if vers.contains_key(&version) {
                println!("{} {}@{} already installed", "✔".green(), pkg_name.cyan(), version.cyan());
                continue;
            }
        }

        install_single(&pkg_name, &version, &repo_mgr, &mut state)?;
    }

    state.save()?;
    Ok(())
}

pub fn install_single(
    pkg_name: &str,
    version: &str,
    repo_mgr: &RepoManager,
    state: &mut State,
) -> Result<()> {
    let index = repo_mgr.build_index()?;
    let checkout_dir = repo_mgr.checkout_package(pkg_name, version, &index)?;

    let manifest = Manifest::load_from_path(checkout_dir.to_str().unwrap())?;

    // Zainstaluj zależności .deb do budowania
    if !manifest.build.deb_deps.is_empty() {
        crate::utils::ensure_deb_packages(&manifest.build.deb_deps)?;
    }

    // Wykonaj budowanie
    let build_script = checkout_dir.join("build.info");
    if build_script.exists() {
        crate::utils::make_executable(&build_script)?;
        crate::sandbox::run_commands(
            checkout_dir.to_str().unwrap(),
                                     &manifest,
                                     &["./build.info".to_string()],
        )?;
    } else if !manifest.build.commands.is_empty() {
        crate::sandbox::run_commands(
            checkout_dir.to_str().unwrap(),
                                     &manifest,
                                     &manifest.build.commands,
        )?;
    }

    let contents_src = checkout_dir.join("contents");
    if !contents_src.exists() {
        anyhow::bail!("No contents/ directory found after build. The package may be malformed.");
    }

    let checksum = crate::utils::compute_dir_hash(&contents_src)?;

    let dest_dir = Path::new(STORE_PATH).join(pkg_name).join(version);
    fs::create_dir_all(&dest_dir)?;

    crate::utils::copy_dir_all(&contents_src, &dest_dir)?;

    if !manifest.runtime.deb_deps.is_empty() {
        crate::utils::ensure_deb_packages(&manifest.runtime.deb_deps)?;
    }

    for bin in &manifest.bins {
        let wrapper_path = Path::new("/usr/bin").join(bin);
        let wrapper_content = format!(
            "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
            std::env::current_exe()?.display(),
                                      pkg_name,
                                      bin
        );
        fs::write(&wrapper_path, wrapper_content)?;
        crate::utils::make_executable(&wrapper_path)?;
    }

    state.update_package(pkg_name, version, &checksum);

    let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
    let _ = fs::remove_file(&current_link);
    std::os::unix::fs::symlink(version, &current_link)?;

    Ok(())
}
