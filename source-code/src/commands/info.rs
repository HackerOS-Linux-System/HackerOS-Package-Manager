use anyhow::{Context, Result};
use colored::Colorize;
use tokio::runtime::Runtime;
use crate::{
    repo::RepoManager,
    state::State,
};

pub fn info(package: String) -> Result<()> {
    let rt = Runtime::new()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let index = repo_mgr.build_index()?;
    let state = State::load()?;

    let pkg = index.get(&package)
    .with_context(|| format!("Package '{}' not found in repository", package))?;

    let installed_ver = state.get_current_version(&package);
    let pinned = if let Some(ver) = &installed_ver {
        state.packages.get(&package)
        .and_then(|vers| vers.get(ver))
        .map(|info| info.pinned)
        .unwrap_or(false)
    } else {
        false
    };

    let latest_ver = pkg.versions.last().map(|v| v.version.clone()).unwrap_or_default();
    let manifest = pkg.versions.last().map(|v| &v.manifest);

    println!("{} Package: {} {}", "→".blue(), package.cyan(), "─".repeat(40));
    if let Some(m) = manifest {
        println!("{} Author:      {}", "  ".blue(), m.authors);
        println!("{} License:     {}", "  ".blue(), m.license);
        println!("{} Description: {}", "  ".blue(), m.summary);
        if !m.long.is_empty() {
            println!("{} Long desc:   {}", "  ".blue(), m.long);
        }
        println!("{} Dependencies:", "  ".blue());
        for (dep, req) in &m.deps {
            println!("    {} ({})", dep.cyan(), req.yellow());
        }
    }

    println!("{} Available versions:", "  ".blue());
    for v in &pkg.versions {
        let marker = if Some(&v.version) == installed_ver.as_ref() {
            " (installed)".green()
        } else {
            "".clear()
        };
        println!("    {}{}", v.version.cyan(), marker);
    }

    if let Some(ver) = installed_ver {
        println!("{} Installed:    {} {}", "  ".blue(), ver.cyan(), if pinned { "(pinned)".yellow() } else { "".clear() });
    } else {
        println!("{} Installed:    {}", "  ".blue(), "No".red());
    }

    println!("{} Latest:       {}", "  ".blue(), latest_ver.green());

    Ok(())
}
