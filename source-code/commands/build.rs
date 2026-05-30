use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn build(name: String) -> Result<()> {
    println!("{} Building hpm package...\n", "→".cyan());

    // ── 1. Walidacja info.hk ─────────────────────────────────────────────────
    let info_hk = Path::new("info.hk");
    if !info_hk.exists() {
        bail!(
            "info.hk not found in current directory.\n\
  Run {} to create a new package from scratch.",
            "hpm create".yellow()
        );
    }

    print!("  {:<40}", "Validating info.hk:".bold());
    let manifest = crate::manifest::Manifest::load_from_path(".")
        .map_err(|e| miette!("info.hk validation failed: {}", e))?;
    println!("{}", "OK".green());

    // Sprawdź wymagane pola
    validate_manifest(&manifest)?;

    // ── 2. Sprawdź strukturę pakietu ──────────────────────────────────────────
    print!("  {:<40}", "Package structure:".bold());
    let has_contents  = Path::new("contents").exists();
    let has_build_toml = Path::new("build.toml").exists();

    if !has_contents && !has_build_toml {
        println!("{}", "ERROR".red().bold());
        bail!(
            "Package needs either contents/ directory or build.toml\n\
  contents/ — pre-built binary files\n\
  build.toml — instructions to download or build from source"
        );
    }
    println!("{}", "OK".green());

    // ── 3. Sprawdź czy binaria są wykonywalne ─────────────────────────────────
    if has_contents {
        print!("  {:<40}", "Binary executability:".bold());
        let mut not_exec = Vec::new();
        for bin_name in &manifest.bins {
            let candidates = [
                format!("contents/bin/{}", bin_name),
                format!("contents/{}", bin_name),
            ];
            for path in &candidates {
                let p = Path::new(path);
                if p.exists() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = fs::metadata(p).into_diagnostic()?.permissions().mode();
                    if mode & 0o111 == 0 {
                        not_exec.push(path.clone());
                    }
                    break;
                }
            }
        }
        if not_exec.is_empty() {
            println!("{}", "OK".green());
        } else {
            println!("{}", "WARNING".yellow());
            for path in &not_exec {
                println!("    {} {} is not executable", "⚠".yellow(), path);
                println!("      Fix: {}", format!("git update-index --chmod=+x {}", path).cyan());
            }
        }
    }

    // ── 4. Sprawdź spójność tagów ─────────────────────────────────────────────
    if !manifest.tags.is_empty() {
        print!("  {:<40}", "Group tags:".bold());
        let valid_chars = manifest.tags.iter().all(|t| {
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
        if valid_chars {
            println!("{} [{}]", "OK".green(),
                manifest.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", ").dimmed());
        } else {
            println!("{}", "WARNING".yellow());
            println!("    Tags should only contain letters, digits, hyphens and underscores.");
        }
    }

    // ── 5. Sprawdź czy wersja pasuje do tagu git ──────────────────────────────
    print!("  {:<40}", "Git tag consistency:".bold());
    let git_tag = get_latest_git_tag();
    match &git_tag {
        Some(tag) => {
            let tag_ver = tag.trim_start_matches('v');
            if tag_ver == manifest.version {
                println!("{} [{}]", "OK".green(), tag.dimmed());
            } else {
                println!("{}", "WARNING".yellow());
                println!("    Latest git tag: {} but info.hk version: {}",
                         tag.yellow(), manifest.version.cyan());
                println!("    Create tag: {}", format!("git tag v{}", manifest.version).cyan());
            }
        }
        None => {
            println!("{}", "NO TAGS".yellow());
            println!("    Create a release tag: {}", format!("git tag v{}", manifest.version).cyan());
        }
    }

    // ── 6. Nazwa archiwum ────────────────────────────────────────────────────
    let pkg_name = if name.is_empty() {
        format!("{}-{}", manifest.name, manifest.version)
    } else {
        name
    };
    let output = format!("{}.hpm", pkg_name);

    // ── 7. Sprawdź arch ──────────────────────────────────────────────────────
    print!("  {:<40}", "Architecture check:".bold());
    let current_arch = std::env::consts::ARCH;
    let declared_arch = manifest.system_specs.get("arch")
        .map(|s| s.as_str())
        .unwrap_or("any");
    if declared_arch == "any" || declared_arch == current_arch
        || (declared_arch == "x86_64" && current_arch == "x86_64")
        || (declared_arch == "aarch64" && current_arch == "aarch64")
    {
        println!("{} [{}]", "OK".green(), declared_arch.dimmed());
    } else {
        println!("{}", "MISMATCH".yellow());
        println!("    Package declares arch={} but building on {}", declared_arch, current_arch);
    }

    // ── 8. Pakowanie ──────────────────────────────────────────────────────────
    println!();
    print!("  {:<40}", format!("Packaging → {}:", output).bold());

    // Wyklucz pliki deweloperskie
    let status = Command::new("tar")
        .args([
            "-I", "zstd",
            "--exclude=.git",
            "--exclude=.github",
            "--exclude=target",
            "--exclude=_build",
            "--exclude=*.o",
            "--exclude=*.d",
            "--exclude=*.tmp",
            "--exclude=.staging-*",
            "-cf", &output,
            ".",
        ])
        .status()
        .into_diagnostic()?;

    if status.success() {
        println!("{}", "OK".green());
    } else {
        println!("{}", "FAILED".red());
        bail!("tar packaging failed");
    }

    // Rozmiar archiwum
    if let Ok(meta) = fs::metadata(&output) {
        let size = meta.len();
        let human = if size > 1_048_576 {
            format!("{:.1} MB", size as f64 / 1_048_576.0)
        } else {
            format!("{:.1} KB", size as f64 / 1024.0)
        };
        println!("  {:<40} {}", "Archive size:".bold(), human.green());
    }

    // ── 9. Podsumowanie ──────────────────────────────────────────────────────
    println!();
    println!("{} Package built: {}", "✔".green(), output.cyan());
    println!();
    println!("{}", "Package summary:".bold());
    println!("  name    = {}", manifest.name.cyan());
    println!("  version = {}", manifest.version.green());
    println!("  author  = {}", manifest.authors.dimmed());
    println!("  license = {}", manifest.license.dimmed());
    if !manifest.bins.is_empty() {
        println!("  bins    = {}", manifest.bins.join(", ").dimmed());
    }
    if !manifest.tags.is_empty() {
        println!("  tags    = {}", manifest.tags.iter()
            .map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", ").dimmed());
    }
    println!();
    println!("{}", "Next steps:".bold());
    println!("  1. {}", format!("git tag v{} && git push origin main --tags", manifest.version).cyan());
    println!("  2. Submit PR to the HPM package index to register your package.");
    println!("     The maintainer will add your repo URL to repo.json.");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest validation
// ---------------------------------------------------------------------------

fn validate_manifest(manifest: &crate::manifest::Manifest) -> Result<()> {
    let mut errors   = Vec::new();
    let mut warnings = Vec::new();

    // Wymagane pola
    if manifest.name.is_empty()    { errors.push("name is empty"); }
    if manifest.version.is_empty() { errors.push("version is empty"); }
    if manifest.authors.is_empty() { warnings.push("authors is empty"); }
    if manifest.license.is_empty() { warnings.push("license is empty"); }
    if manifest.summary.is_empty() { warnings.push("summary is empty (shown in hpm search)"); }

    // Walidacja nazwy pakietu
    if !manifest.name.is_empty() {
        if !manifest.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            errors.push("name contains invalid characters (only a-z, 0-9, -, _ allowed)");
        }
        if manifest.name.starts_with('-') || manifest.name.starts_with('_') {
            errors.push("name must start with a letter or digit");
        }
    }

    // Walidacja wersji (semver-like)
    if !manifest.version.is_empty() {
        let parts: Vec<&str> = manifest.version.split('.').collect();
        if parts.len() < 2 {
            warnings.push("version should follow semver (e.g. 1.0.0)");
        } else if !parts.iter().all(|p| p.parse::<u32>().is_ok() || p.contains('-')) {
            warnings.push("version parts should be numeric");
        }
    }

    // GUI bez desktop sekcji
    if manifest.is_gui && manifest.desktop.display_name.is_empty() {
        warnings.push("GUI package has no [desktop] display_name");
    }

    // Bins ale brak summary
    if !manifest.bins.is_empty() && manifest.summary.is_empty() {
        warnings.push("package has bins but no summary — users won't see description in hpm search");
    }

    // Sprawdź arch w specs
    if let Some(arch) = manifest.system_specs.get("arch") {
        if !["x86_64", "aarch64", "armhf", "i386", "any"].contains(&arch.as_str()) {
            warnings.push("unknown arch value in [specs] — supported: x86_64, aarch64, armhf, i386, any");
        }
    }

    // Pokaż błędy i ostrzeżenia
    for w in &warnings {
        println!("  {} {}", "⚠".yellow(), w);
    }

    if !errors.is_empty() {
        println!();
        for e in &errors {
            println!("  {} {}", "✗".red(), e);
        }
        bail!("info.hk validation failed with {} error(s)", errors.len());
    }

    Ok(())
}

fn get_latest_git_tag() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    } else {
        None
    }
}
