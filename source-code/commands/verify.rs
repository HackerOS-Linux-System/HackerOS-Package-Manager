use miette::{Result, bail, miette};
use colored::Colorize;
use std::path::Path;
use crate::{
    STORE_PATH,
    state::State,
    utils::compute_dir_hash,
};

pub fn verify(package: String) -> Result<()> {
    if package.is_empty() {
        eprintln!("{} Usage: hpm verify <package>", "✗".red());
        std::process::exit(1);
    }
    let state       = State::load()?;
    let current_ver = state.get_current_version(&package)
        .ok_or_else(|| miette!("Package {} not installed", package))?;
    let expected    = state.packages.get(&package)
        .and_then(|vs| vs.get(&current_ver))
        .map(|info| info.checksum.clone())
        .ok_or_else(|| miette!("No checksum in state"))?;

    let pkg_path = Path::new(STORE_PATH).join(&package).join(&current_ver);
    let computed = compute_dir_hash(&pkg_path)?;

    if computed == expected {
        println!("{} Verification OK for {}@{}", "✔".green(), package.cyan(), current_ver.cyan());
        Ok(())
    } else {
        eprintln!("{} Checksum mismatch for {}@{}", "✗".red(), package.cyan(), current_ver.cyan());
        eprintln!("  Expected: {}", &expected[..16.min(expected.len())]);
        eprintln!("  Computed: {}", &computed[..16.min(computed.len())]);
        bail!("Verification failed");
    }
}
