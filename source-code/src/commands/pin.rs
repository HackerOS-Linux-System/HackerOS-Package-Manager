use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use crate::state::State;

pub fn pin(package: String, version: String) -> Result<()> {
    let lock = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let mut state = State::load()?;

    let versions = state.packages.get_mut(&package)
    .ok_or_else(|| miette::miette!("Package '{}' not installed", package))?;

    let info = versions.get_mut(&version)
    .ok_or_else(|| miette::miette!("Version '{}' of package '{}' not installed", version, package))?;

    info.pinned = true;
    state.save()?;

    println!("{} Pinned {}@{}", "✔".green(), package.cyan(), version.cyan());
    Ok(())
}
