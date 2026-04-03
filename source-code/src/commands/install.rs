use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use indicatif::{ProgressBar, ProgressStyle};
use git2::{Repository, build::CheckoutBuilder};
use crate::{
    STORE_PATH,
    manifest::Manifest,
    repo::{RepoManager, BuildConfig, BuildSource},
    state::State,
    utils::{
        acquire_lock, release_lock, compute_dir_hash, copy_dir_all,
        make_executable, compare_versions, download_file,
    },
};

pub fn install(specs: Vec<String>) -> Result<()> {
    if specs.is_empty() {
        eprintln!("{} Usage: hpm install <package>[@<version>]...", "✗".red());
        std::process::exit(1);
    }

    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let mut state = State::load()?;

    for spec in specs {
        let (pkg_name, requested_ver) = if spec.contains('@') {
            let mut parts = spec.splitn(2, '@');
            (
                parts.next().unwrap().to_string(),
             Some(parts.next().unwrap().to_string()),
            )
        } else {
            (spec, None)
        };

        let _pkg_url = repo_mgr
        .get_package_url(&pkg_name)
        .ok_or_else(|| miette::miette!(
            "Package '{}' not found in repository index.\n  Tip: run {} to refresh.",
            pkg_name,
            "hpm refresh".yellow()
        ))?;

        // Skip if already installed at requested version
        if let Some(ver) = &requested_ver {
            if let Some(vers) = state.packages.get(&pkg_name) {
                if vers.contains_key(ver.as_str()) {
                    println!(
                        "{} {}@{} is already installed",
                        "✔".green(), pkg_name.cyan(), ver.cyan()
                    );
                    continue;
                }
            }
        }

        install_single(&pkg_name, requested_ver.as_deref(), &repo_mgr, &mut state)?;
    }

    state.save()?;
    Ok(())
}

/// Install a single package at an optional version (None = latest tag).
pub fn install_single(
    pkg_name: &str,
    version: Option<&str>,
    repo_mgr: &RepoManager,
    state: &mut State,
) -> Result<()> {
    let pkg_url = repo_mgr
    .get_package_url(pkg_name)
    .ok_or_else(|| miette::miette!("Package '{}' not found in repository index", pkg_name))?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
        .template("{spinner:.cyan} {msg}")
        .unwrap(),
    );
    pb.set_message(format!("Fetching {}...", pkg_name.cyan()));

    // Clone or update the package git repository
    let repo_path = repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
    let repo = Repository::open(&repo_path).into_diagnostic()?;
    let tags = repo.tag_names(None).into_diagnostic()?;

    // ── Resolve version from tags ────────────────────────────────────────────
    let (selected_version, commit_oid) = resolve_version(&repo, &tags, version, pkg_name)?;

    pb.set_message(format!(
        "Checking out {}@{}...",
        pkg_name.cyan(), selected_version.green()
    ));

    // Checkout the tag tree into a temp directory
    let commit = repo.find_commit(commit_oid).into_diagnostic()?;
    let tree = commit.tree().into_diagnostic()?;
    let checkout_dir = tempfile::tempdir().into_diagnostic()?;
    let mut checkout_opts = CheckoutBuilder::new();
    checkout_opts.target_dir(checkout_dir.path());
    repo.checkout_tree(tree.as_object(), Some(&mut checkout_opts)).into_diagnostic()?;

    let src_dir = checkout_dir.path();

    // ── Load manifest (info.hk) ──────────────────────────────────────────────
    let manifest = Manifest::load_from_path(src_dir.to_str().unwrap())?;

    // ── Load build config (build.toml) ───────────────────────────────────────
    let build_cfg = BuildConfig::load_from_dir(src_dir);

    // ── Resolve hpm-level dependencies ──────────────────────────────────────
    if !manifest.deps.is_empty() {
        pb.set_message("Resolving dependencies...");
        for (dep_name, dep_req) in &manifest.deps {
            let already_ok = state
            .packages
            .get(dep_name)
            .map(|vers| vers.keys().any(|v| crate::utils::satisfies(v, dep_req)))
            .unwrap_or(false);

            if !already_ok {
                println!(
                    "\n  {} Installing dependency: {}{}",
                    "→".yellow(),
                         dep_name.cyan(),
                         if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) }
                );
                let dep_ver = if dep_req.starts_with(">=")
                || dep_req.starts_with('>')
                || dep_req.starts_with('=')
                {
                    None
                } else if dep_req.is_empty() {
                    None
                } else {
                    Some(dep_req.as_str())
                };
                install_single(dep_name, dep_ver, repo_mgr, state)?;
            }
        }
    }

    // ── Debian build dependencies ────────────────────────────────────────────
    let build_deb_deps: Vec<String> = {
        let mut d = manifest.build.deb_deps.clone();
        if let Some(ref cfg) = build_cfg {
            for dep in &cfg.build_deps {
                if !d.contains(dep) { d.push(dep.clone()); }
            }
        }
        d
    };
    if !build_deb_deps.is_empty() {
        pb.set_message("Installing build dependencies...");
        crate::utils::ensure_deb_packages(&build_deb_deps)?;
    }

    // ── Build / download step ────────────────────────────────────────────────
    // After this block, contents_src must point to a directory ready to copy.
    let contents_src = if let Some(ref cfg) = build_cfg {
        run_build_config(cfg, src_dir, &selected_version, &manifest, &pb, pkg_name)?
    } else {
        // No build.toml: classic layout — run build.info / manifest commands,
        // then use contents/
        run_classic_build(src_dir, &manifest, &pb)?;
        src_dir.join("contents")
    };

    if !contents_src.exists() {
        bail!(
            "No 'contents/' directory found after build for '{}@{}'.\n\
The package must either have a contents/ directory or a build.toml.",
pkg_name, selected_version
        );
    }

    // ── Checksum ─────────────────────────────────────────────────────────────
    pb.set_message("Computing checksum...");
    let checksum = compute_dir_hash(&contents_src)?;

    // ── Copy to store ─────────────────────────────────────────────────────────
    let dest_dir = Path::new(STORE_PATH).join(pkg_name).join(&selected_version);
    fs::create_dir_all(&dest_dir).into_diagnostic()?;
    pb.set_message("Installing files...");
    copy_dir_all(&contents_src, &dest_dir)?;

    // Store the manifest for future reference (remove, verify, etc.)
    let manifest_src = src_dir.join("info.hk");
    if manifest_src.exists() {
        fs::copy(&manifest_src, dest_dir.join("info.hk")).into_diagnostic()?;
    }

    // ── Runtime deb dependencies ─────────────────────────────────────────────
    let runtime_deb_deps: Vec<String> = {
        let mut d = manifest.runtime.deb_deps.clone();
        if let Some(ref cfg) = build_cfg {
            for dep in &cfg.runtime_deps {
                if !d.contains(dep) { d.push(dep.clone()); }
            }
        }
        d
    };
    if !runtime_deb_deps.is_empty() {
        pb.set_message("Installing runtime dependencies...");
        crate::utils::ensure_deb_packages(&runtime_deb_deps)?;
    }

    // ── Create /usr/bin wrappers ─────────────────────────────────────────────
    pb.set_message("Creating binary wrappers...");
    for bin in &manifest.bins {
        let wrapper_path = Path::new("/usr/bin").join(bin);
        let wrapper_content = format!(
            "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
            std::env::current_exe().into_diagnostic()?.display(),
                                      pkg_name,
                                      bin
        );
        fs::write(&wrapper_path, &wrapper_content).into_diagnostic()?;
        make_executable(&wrapper_path)?;
    }

    // ── Update state ─────────────────────────────────────────────────────────
    state.update_package(pkg_name, &selected_version, &checksum);

    let current_link = Path::new(STORE_PATH).join(pkg_name).join("current");
    let _ = fs::remove_file(&current_link);
    std::os::unix::fs::symlink(&selected_version, &current_link).into_diagnostic()?;

    pb.finish_with_message(format!(
        "{} {}@{} installed successfully",
        "✔".green(), pkg_name.cyan(), selected_version.green()
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Version resolution
// ---------------------------------------------------------------------------

fn resolve_version(
    repo: &Repository,
    tags: &git2::string_array::StringArray,
    version: Option<&str>,
    pkg_name: &str,
) -> Result<(String, git2::Oid)> {
    if let Some(v) = version {
        // Find exact tag match (with or without leading 'v')
        let found = tags.iter().flatten()
        .find(|tag| tag.trim_start_matches('v') == v)
        .ok_or_else(|| miette::miette!(
            "Version {} not found in repository tags for '{}'.\n\
Tags must follow the format 'v1.2.3' or '1.2.3'.",
v, pkg_name
        ))?;
        let obj = repo.revparse_single(found).into_diagnostic()?;
        let commit = obj.peel_to_commit().into_diagnostic()?;
        Ok((v.to_string(), commit.id()))
    } else {
        // Pick the highest semver tag
        let mut tag_versions: Vec<(String, git2::Oid)> = Vec::new();
        for tag_name in tags.iter().flatten() {
            let ver_str = tag_name.trim_start_matches('v');
            if let Ok(obj) = repo.revparse_single(tag_name) {
                if let Ok(commit) = obj.peel_to_commit() {
                    tag_versions.push((ver_str.to_string(), commit.id()));
                }
            }
        }

        if tag_versions.is_empty() {
            // No tags: fall back to HEAD, use version from info.hk
            let head = repo.head().into_diagnostic()?;
            let commit = head.peel_to_commit().into_diagnostic()?;
            eprintln!(
                "{} No tags found for '{}', installing from HEAD.",
                "⚠".yellow(), pkg_name
            );
            return Ok(("HEAD".to_string(), commit.id()));
        }

        tag_versions.sort_by(|a, b| compare_versions(&a.0, &b.0));
        let (latest_ver, commit_id) = tag_versions.last().unwrap();
        Ok((latest_ver.clone(), *commit_id))
    }
}

// ---------------------------------------------------------------------------
// Classic build (no build.toml)
// ---------------------------------------------------------------------------

fn run_classic_build(src_dir: &Path, manifest: &Manifest, pb: &ProgressBar) -> Result<()> {
    let build_script = src_dir.join("build.info");
    if build_script.exists() {
        pb.set_message("Running build.info...");
        make_executable(&build_script)?;
        crate::sandbox::run_commands(
            src_dir.to_str().unwrap(),
                                     manifest,
                                     &["./build.info".to_string()],
        )?;
    } else if !manifest.build.commands.is_empty() {
        pb.set_message("Building package...");
        crate::sandbox::run_commands(
            src_dir.to_str().unwrap(),
                                     manifest,
                                     &manifest.build.commands,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// build.toml-driven build
// ---------------------------------------------------------------------------

fn run_build_config(
    cfg: &BuildConfig,
    src_dir: &Path,
    version: &str,
    manifest: &Manifest,
    pb: &ProgressBar,
    pkg_name: &str,
) -> Result<PathBuf> {
    // Prepare the contents/ staging directory
    let contents_dir = src_dir.join("contents");
    fs::create_dir_all(&contents_dir).into_diagnostic()?;

    // Determine destination path inside contents/
    let install_path = if cfg.install_path.is_empty() {
        format!("bin/{}", pkg_name)
    } else {
        cfg.install_path.clone()
    };
    let dest = contents_dir.join(&install_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).into_diagnostic()?;
    }

    match &cfg.source {
        // ── Prebuilt: contents/ is already present in the repo ───────────────
        BuildSource::Prebuilt => {
            pb.set_message("Using prebuilt contents/...");
            // Nothing to do — caller checks contents_dir exists
        }

        // ── Download pre-built binary / archive ──────────────────────────────
        BuildSource::Download { url, binary_path, strip_components } => {
            let resolved_url = url.replace("{version}", version);
            pb.set_message(format!("Downloading {}...", resolved_url.dimmed()));

            let tmp_download = tempfile::NamedTempFile::new().into_diagnostic()?;
            let tmp_path = tmp_download.path().to_str().unwrap().to_string();
            download_file(&resolved_url, &tmp_path)?;

            // Detect if it is a tar archive or a plain binary
            let is_tar = resolved_url.contains(".tar.")
            || resolved_url.ends_with(".tgz")
            || resolved_url.ends_with(".tar");
            let is_zip = resolved_url.ends_with(".zip");

            if is_tar {
                pb.set_message("Extracting archive...");
                let extract_dir = tempfile::tempdir().into_diagnostic()?;
                let mut cmd = std::process::Command::new("tar");
                cmd.arg("-xf").arg(&tmp_path).arg("-C").arg(extract_dir.path());
                if *strip_components > 0 {
                    cmd.arg(format!("--strip-components={}", strip_components));
                }
                let status = cmd.status().into_diagnostic()?;
                if !status.success() {
                    bail!("tar extraction failed");
                }

                if binary_path.is_empty() {
                    // Copy entire extracted tree into contents/
                    copy_dir_all(extract_dir.path(), &contents_dir)?;
                } else {
                    let src = extract_dir.path().join(binary_path);
                    fs::copy(&src, &dest).into_diagnostic()?;
                    make_executable(&dest)?;
                }
            } else if is_zip {
                pb.set_message("Extracting zip...");
                let extract_dir = tempfile::tempdir().into_diagnostic()?;
                let status = std::process::Command::new("unzip")
                .arg("-q")
                .arg(&tmp_path)
                .arg("-d")
                .arg(extract_dir.path())
                .status()
                .into_diagnostic()?;
                if !status.success() {
                    bail!("unzip extraction failed");
                }
                if binary_path.is_empty() {
                    copy_dir_all(extract_dir.path(), &contents_dir)?;
                } else {
                    let src = extract_dir.path().join(binary_path);
                    fs::copy(&src, &dest).into_diagnostic()?;
                    make_executable(&dest)?;
                }
            } else {
                // Plain binary download
                fs::copy(&tmp_path, &dest).into_diagnostic()?;
                make_executable(&dest)?;
            }
        }

        // ── Build from source ────────────────────────────────────────────────
        BuildSource::Build { commands, output } => {
            pb.set_message("Building from source...");

            // Apply extra env vars defined in build.toml
            for (k, v) in &cfg.env {
                std::env::set_var(k, v);
            }

            // Write a temporary shell script and run it in sandbox
            let script_content = commands.join("\n");
            let script_path = src_dir.join("_hpm_build.sh");
            fs::write(&script_path, format!("#!/bin/sh\nset -e\n{}", script_content))
            .into_diagnostic()?;
            make_executable(&script_path)?;

            crate::sandbox::run_commands(
                src_dir.to_str().unwrap(),
                                         manifest,
                                         &["./_hpm_build.sh".to_string()],
            )?;
            let _ = fs::remove_file(&script_path);

            // Collect the output artefact
            let output_path = src_dir.join(output);
            if !output_path.exists() {
                bail!(
                    "Build finished but expected output '{}' not found.\n\
Check the 'output' field in build.toml.",
output
                );
            }
            if output_path.is_dir() {
                copy_dir_all(&output_path, &contents_dir)?;
            } else {
                fs::copy(&output_path, &dest).into_diagnostic()?;
                make_executable(&dest)?;
            }
        }
    }

    Ok(contents_dir)
}
