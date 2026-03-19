use anyhow::{anyhow, Result};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{
    STORE_PATH,
    state::State,
    utils::{acquire_lock, release_lock},
};

pub fn remove(spec: String) -> Result<()> {
    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let mut state = State::load()?;

    let (pkg_name, version) = if spec.contains('@') {
        let mut parts = spec.split('@');
        (parts.next().unwrap().to_string(), Some(parts.next().unwrap().to_string()))
    } else {
        (spec.clone(), None)
    };

    if !state.packages.contains_key(&pkg_name) {
        anyhow::bail!("Package {} not installed", pkg_name);
    }

    if let Some(ver) = version {
        remove_version(&pkg_name, &ver, &mut state)?;
        println!("{} {}@{} removed", "✔".green(), pkg_name.cyan(), ver.cyan());
    } else {
        let versions: Vec<String> = state.packages.get(&pkg_name).unwrap().keys().cloned().collect();
        for ver in versions {
            remove_version(&pkg_name, &ver, &mut state)?;
        }
        println!("{} {} removed", "✔".green(), pkg_name.cyan());
    }

    state.save()?;
    Ok(())
}

pub fn remove_version(pkg_name: &str, version: &str, state: &mut State) -> Result<()> {
    let pkg_path = Path::new(STORE_PATH).join(pkg_name).join(version);
    if !pkg_path.exists() {
        anyhow::bail!("Path {} does not exist", pkg_path.display());
    }

    let manifest = crate::manifest::Manifest::load_from_path(pkg_path.to_str().unwrap())?;
    for bin in &manifest.bins {
        let wrapper_path = Path::new("/usr/bin").join(bin);
        if wrapper_path.exists() {
            fs::remove_file(wrapper_path)?;
        }
    }

    fs::remove_dir_all(&pkg_path)?;
    state.remove_package_version(pkg_name, version);

    let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
    if let Ok(target) = fs::read_link(&current_link) {
        if target == Path::new(version) {
            fs::remove_file(current_link)?;
        }
    }

    Ok(())
}
