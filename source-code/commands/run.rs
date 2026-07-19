use miette::{Result, bail, miette};
use std::fs;
use std::ffi::OsStr;
use std::path::Path;
use colored::Colorize;
use crate::{
    state::State,
    manifest::Manifest,
    sandbox::setup_sandbox,
    commands::install::find_binary_in_dir,
};

pub fn run(package_spec: String, bin: String, extra_args: Vec<String>) -> Result<()> {
    let parts    = package_spec.split('@').collect::<Vec<_>>();
    let pkg_name = parts[0];
    let version  = if parts.len() > 1 { Some(parts[1]) } else { None };

    let state = State::load()?;
    if !state.packages.contains_key(pkg_name) {
        bail!("Package '{}' is not installed. Install with: {}",
              pkg_name, format!("hpm install {}", pkg_name).bright_black());
    }

    let pkg_path = if let Some(ver) = version {
        let path = format!("{}{}/{}", crate::store_path(), pkg_name, ver);
        if !Path::new(&path).exists() {
            bail!("Version {} of package '{}' is not installed", ver, pkg_name);
        }
        path
    } else {
        let current_link = format!("{}{}/current", crate::store_path(), pkg_name);
        let target = fs::read_link(&current_link)
            .map_err(|_| miette!("No current version set for package '{}'", pkg_name))?;
        let ver = target.file_name().and_then(OsStr::to_str)
            .ok_or_else(|| miette!("Invalid current symlink for '{}'", pkg_name))?;
        format!("{}{}/{}", crate::store_path(), pkg_name, ver)
    };

    let pkg_dir = Path::new(&pkg_path);
    crate::squash::ensure_mounted(pkg_dir)?;
    let bin_rel = find_binary_in_dir(pkg_dir, &bin).ok_or_else(|| {
        let files: Vec<_> = walkdir::WalkDir::new(pkg_dir)
            .into_iter().filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.file_name().to_str().map(|n| n != "info.hk").unwrap_or(true))
            .map(|e| {
                e.path().strip_prefix(pkg_dir).unwrap_or(e.path())
                    .to_string_lossy().to_string()
            })
            .collect();
        if files.is_empty() {
            miette!("Package '{}' store is empty. Try reinstalling: {}",
                    pkg_name, format!("sudo hpm install {}", pkg_name).bright_black())
        } else {
            miette!("Binary '{}' not found in package '{}'.\n  Files installed:\n{}\n  \
Hint: check 'bins' field in info.hk matches the actual binary name.",
                    bin, pkg_name,
                    files.iter().map(|f| format!("    {}", f)).collect::<Vec<_>>().join("\n"))
        }
    })?;

    let manifest = Manifest::load_from_path(&pkg_path)
        .unwrap_or_else(|_| Manifest { name: pkg_name.to_string(), ..Default::default() });

    setup_sandbox(&pkg_path, &manifest, false, Some(&bin_rel), extra_args, false)?;
    Ok(())
}
