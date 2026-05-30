use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use crate::{STORE_PATH, state::State, repo::RepoManager};

pub fn diff(args: Vec<String>) -> Result<()> {
    if args.len() < 2 {
        eprintln!("{} Usage: hpm diff <package> <ver1> [<ver2>]", "✗".red());
        eprintln!("  If ver2 is omitted, compares ver1 against currently installed version.");
        std::process::exit(1);
    }

    let pkg_name = &args[0];
    let ver1     = &args[1];
    let state    = State::load()?;

    // ver2 domyślnie = aktualnie zainstalowana
    let ver2 = if args.len() >= 3 {
        args[2].clone()
    } else {
        state.get_current_version(pkg_name)
            .ok_or_else(|| miette!(
                "Package '{}' not installed. Provide two explicit versions:\n  hpm diff {} <ver1> <ver2>",
                pkg_name, pkg_name
            ))?
    };

    println!("{} Comparing {}  {} ↔ {}\n",
             "→".cyan(), pkg_name.bold().cyan(),
             ver1.yellow(), ver2.green());

    let path1 = Path::new(STORE_PATH).join(pkg_name).join(ver1);
    let path2 = Path::new(STORE_PATH).join(pkg_name).join(&ver2);

    let in_store1 = path1.exists();
    let in_store2 = path2.exists();

    // Jeśli nie ma lokalnie — spróbuj pobrać z repo
    if !in_store1 || !in_store2 {
        println!("{} One or both versions not in local store — fetching from repo...", "→".yellow());
    }

    // ── Manifest diff ────────────────────────────────────────────────────────
    println!("{}", "Manifest changes:".bold().underline());

    let m1 = if in_store1 {
        crate::manifest::Manifest::load_from_path(path1.to_str().unwrap()).ok()
    } else {
        fetch_manifest_for_version(pkg_name, ver1)?
    };

    let m2 = if in_store2 {
        crate::manifest::Manifest::load_from_path(path2.to_str().unwrap()).ok()
    } else {
        fetch_manifest_for_version(pkg_name, &ver2)?
    };

    match (&m1, &m2) {
        (Some(old), Some(new)) => {
            diff_manifests(old, new, ver1, &ver2);
        }
        (None, Some(_)) => {
            println!("  {} Version {} manifest not available", "⚠".yellow(), ver1);
        }
        (Some(_), None) => {
            println!("  {} Version {} manifest not available", "⚠".yellow(), ver2);
        }
        (None, None) => {
            println!("  {} Neither version manifest available locally", "⚠".yellow());
        }
    }

    // ── File diff (jeśli oba są lokalnie) ────────────────────────────────────
    if in_store1 && in_store2 {
        println!();
        println!("{}", "File changes:".bold().underline());
        diff_files(&path1, &path2, ver1, &ver2)?;
    } else if !in_store1 && !in_store2 {
        println!();
        println!("{} Install both versions to compare files:", "→".dimmed());
        println!("  hpm install {}@{}", pkg_name, ver1);
        println!("  hpm install {}@{}", pkg_name, ver2);
    } else {
        let missing = if !in_store1 { ver1 } else { &ver2 };
        println!();
        println!("{} Version {} not in store — file diff unavailable.", "→".dimmed(), missing);
        println!("  Install it to compare: {}", format!("hpm install {}@{}", pkg_name, missing).yellow());
    }

    // ── Checksum diff ─────────────────────────────────────────────────────────
    println!();
    println!("{}", "Checksums:".bold().underline());
    let ck1 = state.packages.get(pkg_name)
        .and_then(|vs| vs.get(ver1))
        .map(|i| &i.checksum[..16]);
    let ck2 = state.packages.get(pkg_name)
        .and_then(|vs| vs.get(&ver2))
        .map(|i| &i.checksum[..16]);

    match (ck1, ck2) {
        (Some(c1), Some(c2)) => {
            println!("  {} {} {}", ver1.yellow(), "→".dimmed(), c1.dimmed());
            println!("  {} {} {}", ver2.green(),  "→".dimmed(), c2.dimmed());
            if c1 == c2 {
                println!("  {} Contents appear identical.", "→".yellow());
            }
        }
        _ => println!("  {} Checksum data not available for one or both versions.", "→".dimmed()),
    }

    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest field comparison
// ---------------------------------------------------------------------------

fn diff_manifests(
    old: &crate::manifest::Manifest,
    new: &crate::manifest::Manifest,
    ver1: &str,
    ver2: &str,
) {
    let mut any = false;

    macro_rules! field_diff {
        ($field:expr, $label:expr) => {
            if $field.0 != $field.1 {
                println!("  {} {:<16} {} → {}",
                    "~".yellow(), $label,
                    $field.0.dimmed(), $field.1.cyan());
                any = true;
            }
        };
    }

    field_diff!((&old.version, &new.version),   "version:");
    field_diff!((&old.authors, &new.authors),   "authors:");
    field_diff!((&old.license, &new.license),   "license:");
    field_diff!((&old.summary, &new.summary),   "summary:");

    // Deps
    let deps1: HashMap<&str, &str> = old.deps.iter().map(|(k,v)| (k.as_str(), v.as_str())).collect();
    let deps2: HashMap<&str, &str> = new.deps.iter().map(|(k,v)| (k.as_str(), v.as_str())).collect();
    let all_deps: HashSet<&&str>   = deps1.keys().chain(deps2.keys()).collect();

    for dep in &all_deps {
        let d1 = deps1.get(*dep);
        let d2 = deps2.get(*dep);
        match (d1, d2) {
            (Some(v1), Some(v2)) if v1 != v2 => {
                println!("  {} dep {:<16} {} → {}", "~".yellow(), dep, v1.dimmed(), v2.cyan());
                any = true;
            }
            (None, Some(v2)) => {
                println!("  {} dep {:<16} {} (new dependency)", "+".green(), dep, v2.green());
                any = true;
            }
            (Some(v1), None) => {
                println!("  {} dep {:<16} {} (removed dependency)", "−".red(), dep, v1.red());
                any = true;
            }
            _ => {}
        }
    }

    // Bins
    let bins1: HashSet<&str> = old.bins.iter().map(|s| s.as_str()).collect();
    let bins2: HashSet<&str> = new.bins.iter().map(|s| s.as_str()).collect();
    for b in bins2.difference(&bins1) {
        println!("  {} bin {:<16} (new binary)", "+".green(), b);
        any = true;
    }
    for b in bins1.difference(&bins2) {
        println!("  {} bin {:<16} (removed binary)", "−".red(), b);
        any = true;
    }

    // Tags
    let t1: HashSet<&str> = old.tags.iter().map(|s| s.as_str()).collect();
    let t2: HashSet<&str> = new.tags.iter().map(|s| s.as_str()).collect();
    for t in t2.difference(&t1) {
        println!("  {} tag @{}", "+".green(), t);
        any = true;
    }
    for t in t1.difference(&t2) {
        println!("  {} tag @{}", "−".red(), t);
        any = true;
    }

    // Sandbox changes
    if old.sandbox.network != new.sandbox.network {
        println!("  {} sandbox.network {} → {}",
            "~".yellow(),
            old.sandbox.network.to_string().dimmed(),
            new.sandbox.network.to_string().cyan());
        any = true;
    }
    if old.sandbox.gui != new.sandbox.gui {
        println!("  {} sandbox.gui {} → {}",
            "~".yellow(),
            old.sandbox.gui.to_string().dimmed(),
            new.sandbox.gui.to_string().cyan());
        any = true;
    }

    if !any {
        println!("  {} No manifest changes between {} and {}", "→".dimmed(), ver1.yellow(), ver2.green());
    }
}

// ---------------------------------------------------------------------------
// File tree comparison
// ---------------------------------------------------------------------------

fn diff_files(path1: &Path, path2: &Path, ver1: &str, ver2: &str) -> Result<()> {
    let files1 = collect_files_rel(path1)?;
    let files2 = collect_files_rel(path2)?;

    let all: HashSet<&String> = files1.keys().chain(files2.keys()).collect();
    let mut any = false;

    let mut added   = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for rel in &all {
        let f1 = files1.get(*rel);
        let f2 = files2.get(*rel);
        match (f1, f2) {
            (Some(h1), Some(h2)) => {
                if h1 != h2 {
                    changed.push(rel.as_str());
                }
            }
            (None, Some(_)) => added.push(rel.as_str()),
            (Some(_), None) => removed.push(rel.as_str()),
            _ => {}
        }
    }

    for f in &added {
        println!("  {} {}", "+".green(), f);
        any = true;
    }
    for f in &removed {
        println!("  {} {}", "−".red(), f);
        any = true;
    }
    for f in &changed {
        println!("  {} {}", "~".yellow(), f);
        any = true;
    }

    if !any {
        println!("  {} Files identical between {} and {}", "→".dimmed(), ver1.yellow(), ver2.green());
    } else {
        println!();
        println!("  {} {} added  {} removed  {} modified",
            "Summary:".bold(), added.len().to_string().green(),
            removed.len().to_string().red(), changed.len().to_string().yellow());
    }

    Ok(())
}

/// Zbierz pliki jako mapa: relative_path → sha256_first_8
fn collect_files_rel(base: &Path) -> Result<HashMap<String, String>> {
    use sha2::{Sha256, Digest};
    let mut map = HashMap::new();
    for entry in walkdir::WalkDir::new(base)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let rel  = path.strip_prefix(base)
            .into_diagnostic()?
            .to_string_lossy()
            .to_string();
        // Skip info.hk itself — tracked separately via manifest diff
        if rel == "info.hk" { continue; }
        let data = fs::read(path).into_diagnostic()?;
        let hash = format!("{:x}", Sha256::digest(&data));
        map.insert(rel, hash[..16].to_string());
    }
    Ok(map)
}

/// Pobierz manifest konkretnej wersji z lokalnego repo git.
fn fetch_manifest_for_version(
    pkg_name: &str,
    version: &str,
) -> Result<Option<crate::manifest::Manifest>> {
    let repos_dir = dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("hpm/repos")
        .join(pkg_name);

    if !repos_dir.exists() {
        return Ok(None);
    }

    let repo = git2::Repository::open(&repos_dir).into_diagnostic()?;
    let tags = repo.tag_names(None).into_diagnostic()?;

    let tag_name = tags.iter().flatten()
        .find(|t| t.trim_start_matches('v') == version)
        .map(|t| t.to_string());

    let tag_name = match tag_name {
        Some(t) => t,
        None    => return Ok(None),
    };

    let obj    = repo.revparse_single(&tag_name).into_diagnostic()?;
    let commit = obj.peel_to_commit().into_diagnostic()?;
    let tree   = commit.tree().into_diagnostic()?;

    if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
        let blob    = repo.find_blob(entry.id()).into_diagnostic()?;
        let content = String::from_utf8(blob.content().to_vec()).into_diagnostic()?;
        let tmp     = tempfile::tempdir().into_diagnostic()?;
        fs::write(tmp.path().join("info.hk"), &content).into_diagnostic()?;
        return Ok(crate::manifest::Manifest::load_from_path(tmp.path().to_str().unwrap()).ok());
    }

    Ok(None)
}
