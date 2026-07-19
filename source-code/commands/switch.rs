use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{state::State};

pub fn switch_version(package: String, version: String) -> Result<()> {
    let lock   = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());
    let state  = State::load()?;
    if !state.packages.contains_key(&package) {
        miette::bail!("Package {} not installed", package);
    }
    if !state.packages[&package].contains_key(&version) {
        miette::bail!("Version {} of package {} not installed", version, package);
    }
    let current_link = Path::new(crate::store_path()).join(&package).join("current");
    if current_link.exists() {
        fs::remove_file(&current_link).into_diagnostic()?;
    }
    std::os::unix::fs::symlink(&version, &current_link).into_diagnostic()?;

    // Zaktualizuj wrappery /usr/bin żeby wskazywały na nową wersję
    let ver_dir  = Path::new(crate::store_path()).join(&package).join(&version);
    crate::squash::ensure_mounted(&ver_dir)?;
    let hpm_exe  = crate::commands::install::which_hpm_for_wrappers();
    let wn       = crate::state::WrapperNames::load();
    if let Ok(manifest) = crate::manifest::Manifest::load_from_path(ver_dir.to_str().unwrap()) {
        for bin_name in &manifest.bins {
            let wrapper_name = wn.get(&package, bin_name).unwrap_or(bin_name.as_str());
            let wrapper      = Path::new(crate::bin_dir()).join(wrapper_name);
            if let Some(rel) = crate::commands::install::find_binary_in_dir(&ver_dir, bin_name) {
                let content = format!(
                    "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
                    hpm_exe, package, rel
                );
                if fs::write(&wrapper, &content).is_ok() {
                    crate::utils::make_executable(&wrapper).ok();
                }
            }
        }
    }

    println!("{} Switched {} to version {}", "✔".red(), package.white(), version.white());
    Ok(())
}
