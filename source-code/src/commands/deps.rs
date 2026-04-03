use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::collections::HashSet;
use crate::{
    repo::RepoManager,
    utils::satisfies,
};

pub fn deps(spec: String) -> Result<()> {
    let parts: Vec<&str> = spec.split('@').collect();
    let pkg_name = parts[0];
    let req = if parts.len() > 1 { format!("={}", parts[1]) } else { String::new() };

    let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let index = repo_mgr.build_index()?;

    let repo_pkg = index.get(pkg_name)
    .ok_or_else(|| miette::miette!("Package {} not found", pkg_name))?;

    let chosen_ver = if !req.is_empty() {
        repo_pkg.versions.iter()
        .find(|v| satisfies(&v.version, &req))
        .map(|v| v.version.clone())
        .ok_or_else(|| miette::miette!("No matching version"))?
    } else {
        repo_pkg.versions.last().unwrap().version.clone()
    };

    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut stack = vec![(pkg_name.to_string(), chosen_ver.clone())];
    let mut tree = Vec::new();

    while let Some((pkg, ver)) = stack.pop() {
        if !visited.insert((pkg.clone(), ver.clone())) {
            continue;
        }
        tree.push(format!("{}@{}", pkg, ver));

        let pkg_entry = index.get(&pkg).ok_or_else(|| miette::miette!("Package missing in index"))?;
        let ver_entry = pkg_entry.versions.iter().find(|v| v.version == ver).ok_or_else(|| miette::miette!("Version missing"))?;

        for (dep, dep_req) in &ver_entry.deps {
            let dep_pkg = index.get(dep).ok_or_else(|| miette::miette!("Dependency not found"))?;
            let dep_ver = dep_pkg.versions.iter()
            .find(|v| satisfies(&v.version, dep_req))
            .map(|v| v.version.clone())
            .ok_or_else(|| miette::miette!("No matching dependency version"))?;
            stack.push((dep.clone(), dep_ver));
        }
    }

    println!("{} Dependency tree for {}@{}:", "→".blue(), pkg_name.cyan(), chosen_ver.cyan());
    for line in tree {
        println!("  {}", line);
    }

    Ok(())
}
