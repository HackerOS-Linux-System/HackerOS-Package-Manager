use anyhow::Result;
use colored::Colorize;
use std::fs;
use std::path::Path;

const CACHE_PATH: &str = "/var/cache/hpm/";

pub fn clean_cache() -> Result<()> {
    let cache_dir = Path::new(CACHE_PATH);
    if !cache_dir.exists() {
        println!("{} Cache directory does not exist.", "→".yellow());
        return Ok(());
    }

    let mut removed = 0;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("hpm") {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }

    println!("{} Removed {} cached .hpm files.", "✔".green(), removed);
    Ok(())
}
