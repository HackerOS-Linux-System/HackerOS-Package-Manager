use anyhow::{Context, Result};
use colored::Colorize;
use crate::state::State;

pub fn unpin(package: String) -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let mut state = State::load()?;

    let current_ver = state.get_current_version(&package)
    .with_context(|| format!("Package '{}' not installed", package))?;

    let versions = state.packages.get_mut(&package)
    .context("Package not found in state")?;

    let info = versions.get_mut(&current_ver)
    .context("Current version not found in state")?;

    info.pinned = false;
    state.save()?;

    println!("{} Unpinned {} (current version {})", "✔".green(), package.cyan(), current_ver.cyan());
    Ok(())
}
