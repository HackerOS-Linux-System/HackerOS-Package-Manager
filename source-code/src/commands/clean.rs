use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;

pub fn clean_cache() -> Result<()> {
    let mut removed_repos = 0usize;
    let mut freed_bytes: u64 = 0;

    // Cached git repos in ~/.cache/hpm/repos/
    let repos_dir = dirs::cache_dir()
    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
    .join("hpm/repos");

    if repos_dir.exists() {
        for entry in fs::read_dir(&repos_dir).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();
            if path.is_dir() {
                freed_bytes += dir_size(&path);
                fs::remove_dir_all(&path).into_diagnostic()?;
                removed_repos += 1;
            }
        }
    }

    // Legacy /var/cache/hpm files
    let cache_dir = Path::new(crate::CACHE_DIR);
    let mut removed_files = 0usize;
    if cache_dir.exists() {
        for entry in fs::read_dir(cache_dir).into_diagnostic()? {
            let entry = entry.into_diagnostic()?;
            let path = entry.path();
            if path.is_file() {
                freed_bytes += path.metadata().map(|m| m.len()).unwrap_or(0);
                fs::remove_file(&path).into_diagnostic()?;
                removed_files += 1;
            }
        }
    }

    if removed_repos == 0 && removed_files == 0 {
        println!("{} Cache is already empty.", "✔".green());
    } else {
        println!(
            "{} Cleaned: {} git repo(s), {} file(s) removed — {} freed.",
                 "✔".green(),
                 removed_repos,
                 removed_files,
                 human_bytes(freed_bytes)
        );
    }

    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
    .into_iter()
    .filter_map(|e| e.ok())
    .filter_map(|e| e.metadata().ok())
    .filter(|m| m.is_file())
    .map(|m| m.len())
    .sum()
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
