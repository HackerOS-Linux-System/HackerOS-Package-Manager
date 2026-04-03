use miette::{Result, bail, miette};
use colored::Colorize;
use std::fs;
use std::path::Path;
use std::ffi::OsStr;
use crate::{
    STORE_PATH,
    state::State,
    manifest::Manifest,
    sandbox::setup_sandbox,
};

pub fn run(package_spec: String, bin: String, extra_args: Vec<String>) -> Result<()> {
    let parts: Vec<&str> = package_spec.split('@').collect();
    let pkg_name = parts[0];
    let version = if parts.len() > 1 { Some(parts[1]) } else { None };

    let state = State::load()?;

    if !state.packages.contains_key(pkg_name) {
        bail!("Package {} not installed", pkg_name);
    }

    let pkg_path = if let Some(ver) = version {
        let path = format!("{}{}/{}", STORE_PATH, pkg_name, ver);
        if !Path::new(&path).exists() {
            bail!("Version {} of package {} not installed", ver, pkg_name);
        }
        path
    } else {
        let current_link = format!("{}{}/current", STORE_PATH, pkg_name);
        let target = fs::read_link(&current_link)
        .map_err(|_| miette!("No current version for package {}", pkg_name))?;
        let ver = target.file_name().and_then(OsStr::to_str).unwrap();
        format!("{}{}/{}", STORE_PATH, pkg_name, ver)
    };

    let manifest = Manifest::load_from_path(&pkg_path)?;
    setup_sandbox(&pkg_path, &manifest, false, Some(&bin), extra_args, false)?;
    Ok(())
}
