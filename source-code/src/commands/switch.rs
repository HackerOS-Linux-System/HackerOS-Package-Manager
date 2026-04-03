use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{STORE_PATH, state::State};

pub fn switch_version(package: String, version: String) -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let state = State::load()?;

    if !state.packages.contains_key(&package) {
        miette::bail!("Package {} not installed", package);
    }
    if !state.packages[&package].contains_key(&version) {
        miette::bail!("Version {} of package {} not installed", version, package);
    }

    let current_link = Path::new(STORE_PATH).join(&package).join("current");
    if current_link.exists() {
        fs::remove_file(&current_link).into_diagnostic()?;
    }
    std::os::unix::fs::symlink(&version, &current_link).into_diagnostic()?;

    println!("{} Switched {} to version {}", "✔".green(), package.cyan(), version.cyan());
    Ok(())
}
