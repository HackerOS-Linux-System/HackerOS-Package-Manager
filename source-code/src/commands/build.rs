use anyhow::{Context, Result};
use colored::Colorize;
use std::fs;
use std::process::Command;

pub fn build(name: String) -> Result<()> {
    if !fs::metadata("info.hk").is_ok() {
        anyhow::bail!("info.hk not found in current directory");
    }

    let output = format!("{}.hpm", name);
    let status = Command::new("tar")
    .args(&["-I", "zstd", "-cf", &output, "."])
    .status()
    .context("Failed to execute tar")?;

    if status.success() {
        println!("{} Built {} successfully", "✔".green(), output.cyan());
        Ok(())
    } else {
        anyhow::bail!("Build failed");
    }
}
