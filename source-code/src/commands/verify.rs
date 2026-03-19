use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use crate::{
    STORE_PATH,
    state::State,
    utils::compute_dir_hash,
};

pub fn verify(package: String) -> Result<()> {
    let state = State::load()?;

    let current_ver = state.get_current_version(&package)
    .with_context(|| format!("Package {} not installed", package))?;

    let expected_checksum = state.packages.get(&package)
    .and_then(|vers| vers.get(&current_ver))
    .map(|info| info.checksum.clone())
    .context("No checksum in state")?;

    let pkg_path = Path::new(STORE_PATH).join(&package).join(&current_ver);
    let computed = compute_dir_hash(&pkg_path)?;

    if computed == expected_checksum {
        println!("{} Verification OK for {}@{}", "✔".green(), package.cyan(), current_ver.cyan());
        Ok(())
    } else {
        eprintln!("{} Checksum mismatch for {}@{}", "✗".red(), package.cyan(), current_ver.cyan());
        eprintln!("  Expected: {}", expected_checksum);
        eprintln!("  Computed: {}", computed);
        anyhow::bail!("Verification failed");
    }
}
