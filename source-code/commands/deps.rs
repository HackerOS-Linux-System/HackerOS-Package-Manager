use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::collections::HashSet;
use crate::{
    repo::RepoManager,
    utils::satisfies,
};

pub fn deps(spec: String) -> Result<()> {
    if spec.is_empty() {
        eprintln!("{} Usage: hpm deps <package>[@<version>]", "✗".red());
        std::process::exit(1);
    }
    let parts    = spec.split('@').collect::<Vec<_>>();
    let pkg_name = parts[0];
    let req      = if parts.len() > 1 { format!("={}", parts[1]) } else { String::new() };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?;
    let repo_mgr = rt.block_on(RepoManager::load())?;
    let index    = repo_mgr.build_index()?;

    let repo_pkg = index.get(pkg_name)
        .ok_or_else(|| miette::miette!(
            "Package '{}' not found. Run {} first.",
            pkg_name, "hpm refresh".yellow()
        ))?;

    if repo_pkg.versions.is_empty() {
        eprintln!("{} No versions found for '{}'. Run {} to clone repo.",
                  "⚠".yellow(), pkg_name, "hpm install".yellow());
        return Ok(());
    }

    let chosen_ver = if !req.is_empty() {
        repo_pkg.versions.iter()
            .find(|v| satisfies(&v.version, &req))
            .map(|v| v.version.clone())
            .ok_or_else(|| miette::miette!("No version of '{}' matches '{}'", pkg_name, req))?
    } else {
        repo_pkg.versions.last().unwrap().version.clone()
    };

    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut stack   = vec![(pkg_name.to_string(), chosen_ver.clone(), 0usize)];
    let mut output  = Vec::new();

    while let Some((pkg, ver, depth)) = stack.pop() {
        if !visited.insert((pkg.clone(), ver.clone())) { continue; }

        let indent = "  ".repeat(depth);
        if depth == 0 {
            output.push(format!("{}{}@{}", indent, pkg.cyan(), ver.green()));
        } else {
            output.push(format!("{}└─ {}@{}", indent, pkg.cyan(), ver.green()));
        }

        if let Some(pkg_entry) = index.get(&pkg) {
            if let Some(ver_entry) = pkg_entry.versions.iter().find(|v| v.version == ver) {
                for (dep_name, dep_req) in &ver_entry.deps {
                    if let Some(dep_pkg) = index.get(dep_name) {
                        if let Some(dep_ver_entry) = dep_pkg.versions.iter()
                            .find(|v| satisfies(&v.version, dep_req))
                        {
                            stack.push((dep_name.clone(), dep_ver_entry.version.clone(), depth + 1));
                        } else {
                            let indent2 = "  ".repeat(depth + 1);
                            output.push(format!("{}└─ {}@{} {}",
                                indent2, dep_name.cyan(),
                                dep_req.red(), "(not found)".red()));
                        }
                    } else {
                        let indent2 = "  ".repeat(depth + 1);
                        output.push(format!("{}└─ {} {}",
                            indent2, dep_name.cyan(), "(not in index)".dimmed()));
                    }
                }
            }
        }
    }

    println!("{} Dependency tree for {}@{}:\n",
             "→".blue(), pkg_name.cyan(), chosen_ver.green());
    for line in &output {
        println!("{}", line);
    }
    println!();
    Ok(())
}
