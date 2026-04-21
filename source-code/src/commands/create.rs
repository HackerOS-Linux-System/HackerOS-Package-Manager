use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

pub fn create(name: Option<String>) -> Result<()> {
    println!("\n{} HPM Package Creation Wizard\n", "◆".red());

    // ── Gather information ───────────────────────────────────────────────────
    let pkg_name = if let Some(n) = name {
        n
    } else {
        prompt("Package name", None)?
    };

    validate_pkg_name(&pkg_name)?;

    let version  = prompt("Version", Some("1.0.0"))?;
    let authors  = prompt("Author(s)", None)?;
    let license  = prompt_choice("License", &["MIT", "GPL-2.0", "GPL-3.0", "Apache-2.0", "BSD-2-Clause", "custom"], 0)?;
    let summary  = prompt("Short description (shown in hpm search)", None)?;
    let long_desc = prompt("Long description (shown in hpm info)", Some(""))?;

    let pkg_type = prompt_choice(
        "Package type",
        &["CLI application", "GUI application", "Library / tool (no binary)", "Custom"],
                                 0,
    )?;

    let is_gui = pkg_type == "GUI application";
    let has_binary = pkg_type != "Library / tool (no binary)";

    let bin_name = if has_binary {
        prompt("Binary name", Some(&pkg_name))?
    } else {
        String::new()
    };

    let build_type = if has_binary {
        prompt_choice(
            "How is the binary obtained?",
            &[
                "Already in contents/ (prebuilt)",
                      "Download from URL (GitHub Releases etc.)",
                      "Build from source (cargo / make / cmake)",
            ],
            0,
        )?
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
        let overwrite = prompt_bool(
            &format!("Directory '{}' already exists. Continue anyway?", pkg_name),
                                    false,
        )?;
        if !overwrite {
            println!("{} Aborted.", "→".yellow());
            return Ok(());
        }
    }

    fs::create_dir_all(pkg_dir).into_diagnostic()?;

    if has_binary {
        let bin_dir = pkg_dir.join("contents/bin");
        fs::create_dir_all(&bin_dir).into_diagnostic()?;

        // Create placeholder binary script
        let placeholder = bin_dir.join(&bin_name);
        if !placeholder.exists() && build_type == "Already in contents/ (prebuilt)" {
            fs::write(&placeholder, format!(
                "#!/bin/sh\n# {}\n# Replace this with the actual binary.\necho 'Hello from {}!'\n",
                pkg_name, pkg_name
            )).into_diagnostic()?;
            // Mark as executable in git later via .gitattributes
        }

        if is_gui {
            let icon_dir = pkg_dir.join("contents/icons");
            fs::create_dir_all(&icon_dir).into_diagnostic()?;
            // Create a minimal SVG placeholder icon
            let icon = icon_dir.join(format!("{}.svg", pkg_name));
            if !icon.exists() {
                fs::write(&icon, format!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
                    <rect width="64" height="64" rx="12" fill="#ff2040"/>
                    <text x="32" y="42" font-size="28" text-anchor="middle" fill="white" font-family="sans-serif">{}</text>
                    </svg>"#,
                    pkg_name.chars().next().unwrap_or('?').to_uppercase().next().unwrap_or('?')
                )).into_diagnostic()?;
            }
        }
    }

    // ── Write info.hk ────────────────────────────────────────────────────────
    let info_hk = generate_info_hk(
        &pkg_name, &version, &authors, &license, &summary, &long_desc,
        &bin_name, is_gui, needs_network, &categories,
    );
    fs::write(pkg_dir.join("info.hk"), &info_hk).into_diagnostic()?;

    // ── Write build.toml (if needed) ─────────────────────────────────────────
    if has_binary && build_type != "Already in contents/ (prebuilt)" {
        let build_toml = generate_build_toml(&pkg_name, &bin_name, &build_type);
        fs::write(pkg_dir.join("build.toml"), &build_toml).into_diagnostic()?;
    }

    // ── Write README.md ──────────────────────────────────────────────────────
    let readme = generate_readme(&pkg_name, &summary, &authors, &license, is_gui);
    fs::write(pkg_dir.join("README.md"), &readme).into_diagnostic()?;

    // ── Write .gitignore ─────────────────────────────────────────────────────
    fs::write(pkg_dir.join(".gitignore"), "target/\n*.o\n*.so\n*.tmp\n").into_diagnostic()?;

    // ── Write .gitattributes (ensure binary is executable in git) ────────────
    if has_binary && !bin_name.is_empty() {
        fs::write(
            pkg_dir.join(".gitattributes"),
                  format!("contents/bin/{} text eol=lf\n", bin_name),
        ).into_diagnostic()?;
    }

    // ── Write GitHub Actions CI ──────────────────────────────────────────────
    let gh_dir = pkg_dir.join(".github/workflows");
    fs::create_dir_all(&gh_dir).into_diagnostic()?;
    fs::write(gh_dir.join("hpm-validate.yml"), generate_ci_workflow(&pkg_name))
    .into_diagnostic()?;

    // ── Print summary ────────────────────────────────────────────────────────
    println!("\n{} Package '{}' created successfully!\n", "✔".green(), pkg_name.cyan());

    println!("{}", "Next steps:".bold());
    println!("  1. cd {}", pkg_name.cyan());
    if has_binary && build_type == "Already in contents/ (prebuilt)" {
        println!("  2. Replace {} with your actual binary", format!("contents/bin/{}", bin_name).cyan());
        println!("  3. {}", "git add . && git update-index --chmod=+x contents/bin/".cyan());
    } else if build_type.contains("Build from source") {
        println!("  2. Edit {} with your build commands", "build.toml".cyan());
    } else if build_type.contains("Download") {
        println!("  2. Edit {} with the download URL", "build.toml".cyan());
    }
    if has_binary {
        println!("  {}. Edit {} — fill in the long description", if has_binary { "4" } else { "3" }, "info.hk".cyan());
    }
    println!("  n. {} to create the first release", "git tag v1.0.0 && git push origin main --tags".cyan());
    println!("  n. Submit a PR to add your package to the HPM index\n");

    println!("{} Files created:", "→".blue());
    println!("  {} info.hk", "✔".green());
    if has_binary && build_type != "Already in contents/ (prebuilt)" {
        println!("  {} build.toml", "✔".green());
    }
    if has_binary {
        println!("  {} contents/bin/{} (placeholder)", "✔".green(), bin_name);
    }
    if is_gui {
        println!("  {} contents/icons/{}.svg (placeholder icon)", "✔".green(), pkg_name);
    }
    println!("  {} README.md", "✔".green());
    println!("  {} .gitignore", "✔".green());
    println!("  {} .github/workflows/hpm-validate.yml", "✔".green());

    Ok(())
}

// ---------------------------------------------------------------------------
// Template generators
// ---------------------------------------------------------------------------

fn generate_info_hk(
    name: &str, version: &str, authors: &str, license: &str,
    summary: &str, long_desc: &str,
    bin_name: &str, is_gui: bool, needs_network: bool, categories: &str,
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
        out.push_str(&format!("-> bins.{} => \"\"\n", bin_name));
    }
    out.push_str("\n[description]\n");
    out.push_str(&format!("-> summary => {}\n", summary));
    if !long_desc.is_empty() {
        out.push_str(&format!("-> long    => {}\n", long_desc));
    } else {
        out.push_str(&format!("-> long    => {}\n", summary));
    }
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
        out.push_str(&format!("-> display_name => {}\n",
                              {
                                  let mut c = name.chars();
                                  c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                                  .unwrap_or_default()
                              }
        ));
        out.push_str(&format!("-> icon         => icons/{}.svg\n", name));
        out.push_str(&format!("-> categories   => {}\n",
                              if categories.is_empty() { "Utility;" } else { categories }
        ));
        out.push_str(&format!("-> comment      => {}\n", summary));
    }

    out
}

fn generate_build_toml(pkg_name: &str, bin_name: &str, build_type: &str) -> String {
    if build_type.contains("Download") {
        format!(
            r#"! build.toml for {}
            ! Set the correct URL — {{version}} is replaced with the git tag version

            type         = "download"
            url          = "https://github.com/YOUR_GITHUB_USER/{}/releases/download/v{{version}}/{}-linux-x86_64"
            install_path = "bin/{}"

            ! For tar.gz archives, use:
            ! type             = "download"
            ! url              = "https://github.com/USER/REPO/releases/download/v{{version}}/tool-linux.tar.gz"
            ! binary_path      = "tool/bin/tool"
            ! strip_components = 1
            ! install_path     = "bin/{}"

            runtime_deps = []
            "#,
            pkg_name, pkg_name, pkg_name, bin_name, bin_name
        )
    } else {
        format!(
            r#"! build.toml for {}
            ! Adjust commands to match your build system

            type         = "build"
            commands     = [
            "cargo build --release",
            ]
            output       = "target/release/{}"
            install_path = "bin/{}"

            build_deps   = ["build-essential"]
            runtime_deps = []

            [env]
            ! CARGO_PROFILE_RELEASE_LTO = "true"
            "#,
            pkg_name, bin_name, bin_name
        )
    }
}

fn generate_readme(name: &str, summary: &str, authors: &str, license: &str, is_gui: bool) -> String {
    format!(
        r#"# {}

        {}

        ## Installation

        ```sh
        sudo hpm install {}
        ```

        ## Usage

        ```sh
        {}{}
        ```

        ## Building from source

        This package uses the [HPM package format](https://github.com/HackerOS-Linux-System/HackerOS-Package-Manager).

    ```sh
    git clone https://github.com/YOUR_USER/{}
    cd {}
    # edit contents/bin/{} or build.toml as needed
    git tag v1.0.0
    git push origin main --tags
    ```

    ## Authors

    {}

    ## License

    {}
    "#,
    name, summary, name,
    name,
    if is_gui { " &" } else { " --help" },
        name, name, name,
        authors, license
    )
}

fn generate_ci_workflow(pkg_name: &str) -> String {
    format!(
        r#"# .github/workflows/hpm-validate.yml
        # Validates that the HPM package manifest is well-formed.

        name: Validate HPM Package

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
        run: test -f info.hk && echo "info.hk found" || (echo "ERROR: info.hk missing"; exit 1)

        - name: Check version matches tag
        if: startsWith(github.ref, 'refs/tags/')
        run: |
        TAG="${{github.ref_name}}"
        TAG_VER="${{TAG#v}}"
        HK_VER=$(grep -E '-> version =>' info.hk | head -1 | sed 's/.*=> //' | tr -d ' ')
        if [ "$TAG_VER" != "$HK_VER" ]; then
            echo "ERROR: Tag version ($TAG_VER) does not match info.hk version ($HK_VER)"
            exit 1
            fi
            echo "Version OK: $HK_VER"

            - name: Check contents/ or build.toml exists
            run: |
            if [ -d contents ] || [ -f build.toml ]; then
                echo "Package source OK"
                else
                    echo "ERROR: Package must have contents/ directory or build.toml"
                    exit 1
                    fi

                    - name: Check binary is executable
                    run: |
                    for f in contents/bin/*; do
                        if [ -f "$f" ]; then
                            if [ ! -x "$f" ]; then
                                echo "WARNING: $f is not executable. Run: git update-index --chmod=+x $f"
                                else
                                    echo "OK: $f is executable"
                                    fi
                                    fi
                                    done
                                    "#
    )
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
    Ok(if input.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        input
    })
}

fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("  {} {} {}: ", "?".cyan(), label.bold(), hint.dimmed());
    io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).into_diagnostic()?;
    let input = input.trim().to_lowercase();
    Ok(match input.as_str() {
        "y" | "yes" => true,
       "n" | "no"  => false,
       ""           => default,
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
    let input = input.trim();
    let idx: usize = if input.is_empty() {
        default_idx
    } else {
        input.parse().unwrap_or(default_idx)
    };
    Ok(choices.get(idx).unwrap_or(&choices[default_idx]).to_string())
}

fn validate_pkg_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(miette::miette!("Package name cannot be empty"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(miette::miette!(
            "Package name '{}' contains invalid characters.\n\
Only letters, digits, hyphens and underscores are allowed.",
name
        ));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(miette::miette!("Package name must start with a letter or digit"));
    }
    Ok(())
}
