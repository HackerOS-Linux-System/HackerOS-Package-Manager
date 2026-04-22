// src/commands/create.rs

use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

pub fn create(name: Option<String>) -> Result<()> {
    println!("\n{} HPM Package Creation Wizard\n", "◆".red());

    let pkg_name = if let Some(n) = name { n } else { prompt("Package name", None)? };
    validate_pkg_name(&pkg_name)?;

    let version   = prompt("Version", Some("1.0.0"))?;
    let authors   = prompt("Author(s)", None)?;
    let license   = prompt_choice("License",
        &["MIT", "GPL-2.0", "GPL-3.0", "Apache-2.0", "BSD-2-Clause", "custom"], 0)?;
    let summary   = prompt("Short description (shown in hpm search)", None)?;
    let long_desc = prompt("Long description (shown in hpm info)", Some(&summary))?;

    let pkg_type  = prompt_choice("Package type",
        &["CLI application", "GUI application", "Library / tool (no binary)", "Custom"], 0)?;

    let is_gui      = pkg_type == "GUI application";
    let has_binary  = pkg_type != "Library / tool (no binary)";

    let bin_name = if has_binary {
        prompt("Binary name", Some(&pkg_name))?
    } else {
        String::new()
    };

    let build_type = if has_binary {
        prompt_choice("How is the binary obtained?",
            &[
                "Already in contents/ (prebuilt)",
                "Download from URL (GitHub Releases etc.)",
                "Build from source (cargo / make / cmake)",
            ], 0)?
    } else {
        String::new()
    };

    let needs_network = if is_gui {
        prompt_bool("Does the app need network access?", false)?
    } else {
        false
    };

    let categories = if is_gui {
        prompt("XDG categories (e.g. Graphics;Viewer;)", Some("Utility;"))?
    } else {
        String::new()
    };

    // ── Create directory structure ───────────────────────────────────────────
    let pkg_dir = Path::new(&pkg_name);
    if pkg_dir.exists() {
        if !prompt_bool(
            &format!("Directory '{}' already exists. Continue?", pkg_name), false)? {
            println!("{} Aborted.", "→".yellow());
            return Ok(());
        }
    }

    fs::create_dir_all(pkg_dir).into_diagnostic()?;

    if has_binary {
        let bin_dir = pkg_dir.join("contents/bin");
        fs::create_dir_all(&bin_dir).into_diagnostic()?;

        if !bin_name.is_empty() && build_type == "Already in contents/ (prebuilt)" {
            let placeholder = bin_dir.join(&bin_name);
            if !placeholder.exists() {
                let content = format!(
                    "#!/bin/sh\n# {} — replace with actual binary\necho 'Hello from {}!'\n",
                    pkg_name, pkg_name
                );
                fs::write(&placeholder, content).into_diagnostic()?;
                // Make the placeholder executable immediately
                crate::utils::make_executable(&placeholder)?;
            }
        }

        if is_gui {
            let icon_dir = pkg_dir.join("contents/icons");
            fs::create_dir_all(&icon_dir).into_diagnostic()?;
            let icon = icon_dir.join(format!("{}.svg", pkg_name));
            if !icon.exists() {
                // Use raw string to avoid Rust 2021 prefix issue with "sans-serif"
                let initial = pkg_name.chars().next()
                    .and_then(|c| c.to_uppercase().next())
                    .unwrap_or('?');
                let svg = format!(
                    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" rx="12" fill="#ff2040"/>
  <text x="32" y="42" font-size="28" text-anchor="middle" fill="white" font-family="sans-serif">{}</text>
</svg>"##,
                    initial
                );
                fs::write(&icon, svg).into_diagnostic()?;
            }
        }
    }

    // ── info.hk ─────────────────────────────────────────────────────────────
    let info_hk = generate_info_hk(
        &pkg_name, &version, &authors, &license, &summary, &long_desc,
        &bin_name, is_gui, needs_network, &categories,
        &build_type,
    );
    fs::write(pkg_dir.join("info.hk"), &info_hk).into_diagnostic()?;

    // ── build.toml ──────────────────────────────────────────────────────────
    if has_binary && build_type != "Already in contents/ (prebuilt)" {
        let build_toml = generate_build_toml(&pkg_name, &bin_name, &build_type);
        fs::write(pkg_dir.join("build.toml"), &build_toml).into_diagnostic()?;
    }

    // ── README.md ───────────────────────────────────────────────────────────
    fs::write(pkg_dir.join("README.md"),
        generate_readme(&pkg_name, &summary, &authors, &license, is_gui))
        .into_diagnostic()?;

    // ── .gitignore ──────────────────────────────────────────────────────────
    fs::write(pkg_dir.join(".gitignore"),
        "target/\n*.o\n*.so\n*.tmp\n.staging-*\n")
        .into_diagnostic()?;

    // ── .gitattributes ──────────────────────────────────────────────────────
    if has_binary && !bin_name.is_empty() {
        fs::write(pkg_dir.join(".gitattributes"),
            format!("contents/bin/* text eol=lf\n"))
            .into_diagnostic()?;
    }

    // ── GitHub Actions CI ────────────────────────────────────────────────────
    let gh_dir = pkg_dir.join(".github/workflows");
    fs::create_dir_all(&gh_dir).into_diagnostic()?;
    fs::write(gh_dir.join("hpm-validate.yml"), generate_ci_workflow())
        .into_diagnostic()?;

    // ── Summary ──────────────────────────────────────────────────────────────
    println!("\n{} Package '{}' created!\n", "✔".green(), pkg_name.cyan());
    println!("{}", "Next steps:".bold());
    println!("  1. cd {}", pkg_name.cyan());
    if has_binary && build_type == "Already in contents/ (prebuilt)" {
        println!("  2. Replace {} with your actual binary", format!("contents/bin/{}", bin_name).cyan());
        println!("  3. Make it executable in git:");
        println!("     {}", format!("git update-index --chmod=+x contents/bin/{}", bin_name).cyan());
        println!("     (hpm also auto-detects shebang scripts and chmod +x them)");
    }
    println!("  n. git tag v1.0.0 && git push origin main --tags");
    println!("  n. Submit PR to add package to HPM index\n");

    Ok(())
}

// ---------------------------------------------------------------------------
// Template generators
// ---------------------------------------------------------------------------

fn generate_info_hk(
    name: &str, version: &str, authors: &str, license: &str,
    summary: &str, long_desc: &str,
    bin_name: &str, is_gui: bool, needs_network: bool, categories: &str,
    build_type: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("! {}\n! Generated by hpm create\n\n", name));
    out.push_str("[metadata]\n");
    out.push_str(&format!("-> name    => {}\n", name));
    out.push_str(&format!("-> version => {}\n", version));
    out.push_str(&format!("-> authors => {}\n", authors));
    out.push_str(&format!("-> license => {}\n", license));
    if is_gui { out.push_str("-> gui     => true\n"); }
    if !bin_name.is_empty() {
        // If prebuilt, declare explicit path so hpm knows where to look
        if build_type == "Already in contents/ (prebuilt)" {
            out.push_str(&format!("-> bins.{} => \"bin/{}\"\n", bin_name, bin_name));
        } else {
            out.push_str(&format!("-> bins.{} => \"\"\n", bin_name));
        }
    }
    out.push_str("\n[description]\n");
    out.push_str(&format!("-> summary => {}\n", summary));
    out.push_str(&format!("-> long    => {}\n", long_desc));
    out.push_str("\n[sandbox]\n");
    out.push_str(&format!("-> network  => {}\n", needs_network));
    out.push_str(&format!("-> gui      => {}\n", is_gui));
    out.push_str("-> full_gui => false\n");
    out.push_str("-> dev      => false\n");
    out.push_str("-> disabled => false\n");
    out.push_str("-> filesystem => {}\n");
    out.push_str("\n[build]\n");
    out.push_str("-> commands => {}\n");
    out.push_str("-> deb_deps => {}\n");
    out.push_str("\n[runtime]\n");
    out.push_str("-> deb_deps => {}\n");

    if is_gui {
        out.push_str("\n[desktop]\n");
        let display = {
            let mut c = name.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        };
        out.push_str(&format!("-> display_name => {}\n", display));
        out.push_str(&format!("-> icon         => icons/{}.svg\n", name));
        out.push_str(&format!("-> categories   => {}\n",
            if categories.is_empty() { "Utility;" } else { categories }));
        out.push_str(&format!("-> comment      => {}\n", summary));
    }
    out
}

fn generate_build_toml(pkg_name: &str, bin_name: &str, build_type: &str) -> String {
    if build_type.contains("Download") {
        format!(
            "! build.toml for {}\n\
             type         = \"download\"\n\
             url          = \"https://github.com/YOUR_USER/{}/releases/download/v{{version}}/{}-linux-x86_64\"\n\
             install_path = \"bin/{}\"\n\
             runtime_deps = []\n",
            pkg_name, pkg_name, pkg_name, bin_name
        )
    } else {
        format!(
            "! build.toml for {}\n\
             type         = \"build\"\n\
             commands     = [\"cargo build --release\"]\n\
             output       = \"target/release/{}\"\n\
             install_path = \"bin/{}\"\n\
             build_deps   = [\"build-essential\"]\n\
             runtime_deps = []\n",
            pkg_name, bin_name, bin_name
        )
    }
}

fn generate_readme(name: &str, summary: &str, authors: &str, license: &str, is_gui: bool) -> String {
    format!(
        "# {}\n\n{}\n\n## Installation\n\n```sh\nsudo hpm install {}\n```\n\n\
         ## Usage\n\n```sh\n{}{}\n```\n\n## Authors\n\n{}\n\n## License\n\n{}\n",
        name, summary, name, name,
        if is_gui { " &" } else { " --help" },
        authors, license
    )
}

fn generate_ci_workflow() -> String {
    r#"name: Validate HPM Package
on:
  push:
    branches: [main]
    tags: ['v*']
  pull_request:
    branches: [main]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Check info.hk exists
        run: test -f info.hk || (echo "ERROR: info.hk missing"; exit 1)

      - name: Check version matches tag
        if: startsWith(github.ref, 'refs/tags/')
        run: |
          TAG="${GITHUB_REF_NAME#v}"
          HK_VER=$(grep -E '-> version =>' info.hk | head -1 | sed 's/.*=> //' | tr -d ' ')
          [ "$TAG" = "$HK_VER" ] || (echo "ERROR: tag $TAG != info.hk $HK_VER"; exit 1)

      - name: Check package source exists
        run: |
          [ -d contents ] || [ -f build.toml ] || (echo "ERROR: need contents/ or build.toml"; exit 1)

      - name: Check binaries are executable
        run: |
          for f in contents/bin/*; do
            [ -f "$f" ] && [ ! -x "$f" ] && echo "WARNING: $f not executable (git update-index --chmod=+x $f)"
          done
          exit 0
"#.to_string()
}

// ---------------------------------------------------------------------------
// Interactive prompt helpers
// ---------------------------------------------------------------------------

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    let default_str = default.map(|d| format!(" [{}]", d)).unwrap_or_default();
    print!("  {} {}{}: ", "?".cyan(), label.bold(), default_str.dimmed());
    io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).into_diagnostic()?;
    let input = input.trim().to_string();
    Ok(if input.is_empty() { default.unwrap_or("").to_string() } else { input })
}

fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("  {} {} {}: ", "?".cyan(), label.bold(), hint.dimmed());
    io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).into_diagnostic()?;
    Ok(match input.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no"  => false,
        _            => default,
    })
}

fn prompt_choice(label: &str, choices: &[&str], default_idx: usize) -> Result<String> {
    println!("  {} {}:", "?".cyan(), label.bold());
    for (i, choice) in choices.iter().enumerate() {
        if i == default_idx {
            println!("    {} {} {}", format!("[{}]", i).cyan(), choice, "(default)".dimmed());
        } else {
            println!("    {} {}", format!("[{}]", i).dimmed(), choice);
        }
    }
    print!("  Choice [{}]: ", default_idx.to_string().cyan());
    io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).into_diagnostic()?;
    let idx = input.trim().parse::<usize>().unwrap_or(default_idx);
    Ok(choices.get(idx).unwrap_or(&choices[default_idx]).to_string())
}

fn validate_pkg_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(miette::miette!("Package name cannot be empty"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(miette::miette!(
            "Package name '{}' contains invalid characters.\n\
             Only letters, digits, hyphens and underscores are allowed.", name
        ));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(miette::miette!("Package name must start with a letter or digit"));
    }
    Ok(())
}
