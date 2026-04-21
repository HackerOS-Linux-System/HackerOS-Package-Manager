use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use git2::{Repository, Oid, Tree};
use crate::{
    STORE_PATH,
    manifest::{Manifest, DesktopInfo},
    repo::{RepoManager, BuildConfig, BuildSource},
    state::{State, split_pkg_ver},
    utils::{
        acquire_lock, release_lock, compute_dir_hash, copy_dir_all,
        make_executable, compare_versions, download_file,
    },
};

const DESKTOP_DIR: &str = "/usr/share/applications";
const ICON_DIR:    &str = "/usr/share/icons/hicolor";
const PIXMAP_DIR:  &str = "/usr/share/pixmaps";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn install(specs: Vec<String>) -> Result<()> {
    if specs.is_empty() {
        eprintln!("{} Usage: hpm install <package>[@<version>]...", "✗".red());
        std::process::exit(1);
    }

    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let mut state = State::load()?;

    // Snapshot state before we start (for rollback if something goes wrong)
    let spec_desc = specs.join(", ");
    state.push_snapshot(&format!("pre-install {}", spec_desc));

    let mut any_installed = false;

    for spec in &specs {
        let (pkg_name, requested_ver) = if spec.contains('@') {
            let mut parts = spec.splitn(2, '@');
            (parts.next().unwrap().to_string(), Some(parts.next().unwrap().to_string()))
        } else {
            (spec.clone(), None)
        };

        let _pkg_url = repo_mgr.get_package_url(&pkg_name)
        .ok_or_else(|| miette::miette!(
            "Package '{}' not found in repository index.\n  Run {} to refresh.",
            pkg_name, "hpm refresh".yellow()
        ))?;

        if let Some(ver) = &requested_ver {
            if let Some(vers) = state.packages.get(&pkg_name) {
                if vers.contains_key(ver.as_str()) {
                    println!("{} {}@{} is already installed",
                             "✔".green(), pkg_name.cyan(), ver.cyan());
                    continue;
                }
            }
        }

        install_single(&pkg_name, requested_ver.as_deref(), &repo_mgr, &mut state, true)?;
        any_installed = true;
    }

    if any_installed {
        state.save()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Install a single package (atomic staging → store)
// ---------------------------------------------------------------------------

pub fn install_single(
    pkg_name: &str,
    version: Option<&str>,
    repo_mgr: &RepoManager,
    state: &mut State,
    manually_installed: bool,
) -> Result<()> {
    let pkg_url = repo_mgr.get_package_url(pkg_name)
    .ok_or_else(|| miette::miette!("Package '{}' not found", pkg_name))?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner()
    .template("{spinner:.red} {msg}").unwrap());
    pb.set_message(format!("Fetching {}...", pkg_name.cyan()));

    // Clone / update git repo
    let repo_path = repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
    let repo = Repository::open(&repo_path).into_diagnostic()?;
    let tags = repo.tag_names(None).into_diagnostic()?;

    let (selected_version, commit_oid) = resolve_version(&repo, &tags, version, pkg_name)?;

    pb.set_message(format!("Extracting {}@{}...", pkg_name.cyan(), selected_version.green()));

    // Extract into temp dir (no checkout conflicts)
    let checkout_dir = tempfile::tempdir().into_diagnostic()?;
    let commit = repo.find_commit(commit_oid).into_diagnostic()?;
    let tree = commit.tree().into_diagnostic()?;
    extract_tree(&repo, &tree, checkout_dir.path())?;

    let src_dir = checkout_dir.path();

    pb.set_message("Reading manifest...");
    let manifest = Manifest::load_from_path(src_dir.to_str().unwrap())?;
    let build_cfg = BuildConfig::load_from_dir(src_dir);

    // Conflict check
    let conflict_violations = state.check_conflicts(pkg_name, &manifest.conflicts);
    if !conflict_violations.is_empty() {
        bail!(
            "Cannot install '{}': package conflicts detected:\n{}",
            pkg_name,
            conflict_violations.iter().map(|v| format!("  ✗ {}", v)).collect::<Vec<_>>().join("\n")
        );
    }

    // Resolve hpm deps (auto-install missing ones)
    if !manifest.deps.is_empty() {
        pb.set_message("Resolving dependencies...");
        for (dep_name, dep_req) in &manifest.deps {
            let already_ok = state.packages.get(dep_name)
            .map(|vers| vers.keys().any(|v| crate::utils::satisfies(v, dep_req)))
            .unwrap_or(false);

            if !already_ok {
                println!("\n  {} Installing dependency: {}{}",
                         "→".yellow(), dep_name.cyan(),
                         if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) }
                );
                let dep_ver = if dep_req.is_empty() || dep_req.starts_with(">=")
                || dep_req.starts_with('>') || dep_req.starts_with('=') { None }
                else { Some(dep_req.as_str()) };
                // Auto-installed = false for manually specified, true for deps
                install_single(dep_name, dep_ver, repo_mgr, state, false)?;
            }
        }
    }

    // Debian build deps
    let mut build_deb_deps = manifest.build.deb_deps.clone();
    if let Some(ref cfg) = build_cfg {
        for dep in &cfg.build_deps {
            if !build_deb_deps.contains(dep) { build_deb_deps.push(dep.clone()); }
        }
    }
    if !build_deb_deps.is_empty() {
        pb.set_message("Installing build dependencies...");
        crate::utils::ensure_deb_packages(&build_deb_deps)?;
    }

    // Build step
    let contents_src = if let Some(ref cfg) = build_cfg {
        run_build_config(cfg, src_dir, &selected_version, &manifest, &pb, pkg_name)?
    } else {
        run_classic_build(src_dir, &manifest, &pb)?;
        src_dir.join("contents")
    };

    if !contents_src.exists() {
        bail!(
            "No 'contents/' directory found for '{}@{}'.\n\
The package must have a contents/ directory or a build.toml.",
pkg_name, selected_version
        );
    }

    pb.set_message("Computing checksum...");
    let checksum = compute_dir_hash(&contents_src)?;

    // ── ATOMIC STAGING ───────────────────────────────────────────────────────
    // Write to a staging dir first; only move to final location on success.
    let dest_dir = Path::new(STORE_PATH).join(pkg_name).join(&selected_version);
    let staging_dir = Path::new(STORE_PATH).join(pkg_name)
    .join(format!(".staging-{}", selected_version));

    // Clean up any previous failed staging
    if staging_dir.exists() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    fs::create_dir_all(&staging_dir).into_diagnostic()?;

    // Install to staging
    pb.set_message("Installing files (staging)...");
    let result = (|| -> Result<()> {
        copy_dir_all(&contents_src, &staging_dir)?;

        // Copy manifest
        let manifest_src = src_dir.join("info.hk");
        if manifest_src.exists() {
            fs::copy(&manifest_src, staging_dir.join("info.hk")).into_diagnostic()?;
        }

        // Runtime deb deps
        let mut runtime_deb_deps = manifest.runtime.deb_deps.clone();
        if let Some(ref cfg) = build_cfg {
            for dep in &cfg.runtime_deps {
                if !runtime_deb_deps.contains(dep) { runtime_deb_deps.push(dep.clone()); }
            }
        }
        if !runtime_deb_deps.is_empty() {
            pb.set_message("Installing runtime dependencies...");
            crate::utils::ensure_deb_packages(&runtime_deb_deps)?;
        }

        Ok(())
    })();

    if let Err(e) = result {
        // Rollback: remove staging dir
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(e);
    }

    // ── COMMIT: rename staging → final ───────────────────────────────────────
    if dest_dir.exists() {
        fs::remove_dir_all(&dest_dir).into_diagnostic()?;
    }
    fs::rename(&staging_dir, &dest_dir).into_diagnostic()?;

    // Runtime deb deps (after commit)
    let mut runtime_deb_deps = manifest.runtime.deb_deps.clone();
    if let Some(ref cfg) = build_cfg {
        for dep in &cfg.runtime_deps {
            if !runtime_deb_deps.contains(dep) { runtime_deb_deps.push(dep.clone()); }
        }
    }

    // /usr/bin wrappers
    pb.set_message("Creating binary wrappers...");
    let hpm_exe = std::env::current_exe().into_diagnostic()?;
    for bin_name in &manifest.bins {
        match find_binary_in_dir(&dest_dir, bin_name) {
            Some(rel) => {
                let wrapper_path = Path::new("/usr/bin").join(bin_name);
                let content = format!(
                    "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
                    hpm_exe.display(), pkg_name, rel
                );
                fs::write(&wrapper_path, &content).into_diagnostic()?;
                make_executable(&wrapper_path)?;
            }
            None => {
                let found = list_executables(&dest_dir);
                eprintln!("{} Binary '{}' not found in installed files.", "⚠".yellow(), bin_name);
                if !found.is_empty() {
                    eprintln!("  Executables found:");
                    for f in &found {
                        eprintln!("    {}",
                                  f.strip_prefix(&dest_dir).unwrap_or(f).display());
                    }
                    eprintln!("  Fix: rename the binary in contents/ to match '{}', or update bins in info.hk.", bin_name);
                } else {
                    eprintln!("  The contents/ directory has no executable files.");
                    eprintln!("  Make sure binaries have chmod +x in the git repository (git update-index --chmod=+x).");
                }
            }
        }
    }

    // Desktop integration for GUI apps
    if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
        pb.set_message("Installing desktop integration...");
        install_desktop_integration(&dest_dir, &manifest, pkg_name, &hpm_exe.display().to_string())?;
    }

    // Build depends_on set for state
    let depends_on: HashSet<String> = manifest.deps.iter()
    .map(|(name, _)| {
        state.get_current_version(name)
        .map(|ver| format!("{}@{}", name, ver))
        .unwrap_or_else(|| name.clone())
    })
    .collect();

    // Build conflicts set for state
    let conflicts_with: HashSet<String> = manifest.conflicts.iter()
    .cloned().collect();

    state.update_package(
        pkg_name, &selected_version, &checksum,
        manually_installed, depends_on, conflicts_with,
    );

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
// Desktop integration
// ---------------------------------------------------------------------------

fn install_desktop_integration(
    dest_dir: &Path,
    manifest: &Manifest,
    pkg_name: &str,
    hpm_exe: &str,
) -> Result<()> {
    let desktop = &manifest.desktop;
    let icon_name = install_icon(dest_dir, manifest, pkg_name)?;

    fs::create_dir_all(DESKTOP_DIR).into_diagnostic()?;
    let desktop_file_path = Path::new(DESKTOP_DIR).join(format!("{}.desktop", pkg_name));

    // Use custom .desktop if shipped
    if !desktop.desktop_file.is_empty() {
        let custom = dest_dir.join(&desktop.desktop_file);
        if custom.exists() {
            fs::copy(&custom, &desktop_file_path).into_diagnostic()?;
            patch_desktop_exec(&desktop_file_path, hpm_exe, pkg_name, manifest)?;
            return Ok(());
        }
    }
    if let Some(found) = find_file_by_ext(dest_dir, "desktop") {
        fs::copy(&found, &desktop_file_path).into_diagnostic()?;
        patch_desktop_exec(&desktop_file_path, hpm_exe, pkg_name, manifest)?;
        return Ok(());
    }

    // Auto-generate
    let bin_name = manifest.bins.first().map(|s| s.as_str()).unwrap_or(pkg_name);
    let display_name = if !desktop.display_name.is_empty() {
        desktop.display_name.clone()
    } else {
        let mut c = pkg_name.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    let categories = if !desktop.categories.is_empty() {
        desktop.categories.clone()
    } else {
        "Utility;".to_string()
    };
    let comment = if !desktop.comment.is_empty() {
        desktop.comment.clone()
    } else {
        manifest.summary.clone()
    };

    let exec_cmd = format!("{} run {} {}", hpm_exe, pkg_name, bin_name);
    let mut content = format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment={}\nExec={} %F\nCategories={}\nTerminal={}\n",
        display_name, comment, exec_cmd, categories,
        if manifest.is_gui { "false" } else { "true" }
    );
    if !icon_name.is_empty() { content.push_str(&format!("Icon={}\n", icon_name)); }
    if desktop.nodisplay { content.push_str("NoDisplay=true\n"); }
    if !desktop.mime_types.is_empty() { content.push_str(&format!("MimeType={}\n", desktop.mime_types)); }
    if !desktop.keywords.is_empty() { content.push_str(&format!("Keywords={}\n", desktop.keywords)); }

    fs::write(&desktop_file_path, content).into_diagnostic()?;
    let _ = std::process::Command::new("update-desktop-database").arg(DESKTOP_DIR).status();
    Ok(())
}

fn install_icon(dest_dir: &Path, manifest: &Manifest, pkg_name: &str) -> Result<String> {
    let icon_rel = &manifest.desktop.icon;
    let icon_src = if !icon_rel.is_empty() {
        let p = dest_dir.join(icon_rel);
        if p.exists() { Some(p) } else { None }
    } else {
        let candidates = [
            dest_dir.join(format!("icons/{}.png", pkg_name)),
            dest_dir.join(format!("icons/{}.svg", pkg_name)),
            dest_dir.join(format!("{}.png", pkg_name)),
        ];
        candidates.into_iter().find(|p| p.exists())
        .or_else(|| find_file_by_ext(dest_dir, "png"))
        .or_else(|| find_file_by_ext(dest_dir, "svg"))
    };

    if let Some(src) = icon_src {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
        if ext == "svg" {
            let target_dir = Path::new(ICON_DIR).join("scalable/apps");
            fs::create_dir_all(&target_dir).into_diagnostic()?;
            fs::copy(&src, target_dir.join(format!("{}.svg", pkg_name))).into_diagnostic()?;
        } else {
            let target_dir = Path::new(ICON_DIR).join("256x256/apps");
            fs::create_dir_all(&target_dir).into_diagnostic()?;
            fs::copy(&src, target_dir.join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
            fs::create_dir_all(PIXMAP_DIR).into_diagnostic()?;
            fs::copy(&src, Path::new(PIXMAP_DIR).join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
        }
        let _ = std::process::Command::new("gtk-update-icon-cache")
        .args(["-f", "-t", ICON_DIR]).status();
        return Ok(pkg_name.to_string());
    }
    Ok(String::new())
}

fn patch_desktop_exec(path: &Path, hpm_exe: &str, pkg_name: &str, manifest: &Manifest) -> Result<()> {
    let content = fs::read_to_string(path).into_diagnostic()?;
    let bin_name = manifest.bins.first().map(|s| s.as_str()).unwrap_or(pkg_name);
    let new_exec = format!("{} run {} {}", hpm_exe, pkg_name, bin_name);
    let patched: String = content.lines().map(|line| {
        if line.starts_with("Exec=") {
            let suffix = line.trim_start_matches("Exec=")
            .split_whitespace().skip(1)
            .filter(|t| t.starts_with('%'))
            .collect::<Vec<_>>().join(" ");
            if suffix.is_empty() { format!("Exec={}", new_exec) }
            else { format!("Exec={} {}", new_exec, suffix) }
        } else { line.to_string() }
    }).collect::<Vec<_>>().join("\n");
    fs::write(path, patched + "\n").into_diagnostic()?;
    Ok(())
}

fn find_file_by_ext(dir: &Path, ext: &str) -> Option<PathBuf> {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_file_by_ext(&path, ext) { return Some(found); }
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                return Some(path);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

pub fn find_binary_in_dir(pkg_dir: &Path, bin_name: &str) -> Option<String> {
    if pkg_dir.join("bin").join(bin_name).exists() {
        return Some(format!("bin/{}", bin_name));
    }
    if pkg_dir.join(bin_name).exists() {
        return Some(bin_name.to_string());
    }
    find_recursive_rel(pkg_dir, pkg_dir, bin_name)
}

fn find_recursive_rel(base: &Path, dir: &Path, name: &str) -> Option<String> {
    let rd = fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_recursive_rel(base, &path, name) { return Some(found); }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            let rel = path.strip_prefix(base).ok()?;
            return Some(rel.to_string_lossy().to_string());
        }
    }
    None
}

fn list_executables(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    collect_exec(dir, &mut result);
    result
}

fn collect_exec(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() { collect_exec(&path, out); }
            else if let Ok(meta) = path.metadata() {
                if meta.permissions().mode() & 0o111 != 0 { out.push(path); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Git tree extraction
// ---------------------------------------------------------------------------

fn extract_tree(repo: &Repository, tree: &Tree, dest: &Path) -> Result<()> {
    for entry in tree.iter() {
        let name = match entry.name() { Some(n) => n, None => continue };
        let entry_path = dest.join(name);
        match entry.kind() {
            Some(git2::ObjectType::Blob) => {
                let blob = repo.find_blob(entry.id()).into_diagnostic()?;
                if let Some(parent) = entry_path.parent() {
                    fs::create_dir_all(parent).into_diagnostic()?;
                }
                fs::write(&entry_path, blob.content()).into_diagnostic()?;
                if entry.filemode() == 0o100755 { make_executable(&entry_path)?; }
            }
            Some(git2::ObjectType::Tree) => {
                fs::create_dir_all(&entry_path).into_diagnostic()?;
                let subtree = repo.find_tree(entry.id()).into_diagnostic()?;
                extract_tree(repo, &subtree, &entry_path)?;
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Classic build
// ---------------------------------------------------------------------------

fn run_classic_build(src_dir: &Path, manifest: &Manifest, pb: &ProgressBar) -> Result<()> {
    let build_script = src_dir.join("build.info");
    if build_script.exists() {
        pb.set_message("Running build.info...");
        make_executable(&build_script)?;
        crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest,
                                     &["./build.info".to_string()])?;
    } else if !manifest.build.commands.is_empty() {
        pb.set_message("Building package...");
        crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest,
                                     &manifest.build.commands)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// build.toml build
// ---------------------------------------------------------------------------

fn run_build_config(
    cfg: &BuildConfig,
    src_dir: &Path,
    version: &str,
    manifest: &Manifest,
    pb: &ProgressBar,
    pkg_name: &str,
) -> Result<PathBuf> {
    let contents_dir = src_dir.join("contents");
    fs::create_dir_all(&contents_dir).into_diagnostic()?;

    let install_path = if cfg.install_path.is_empty() {
        format!("bin/{}", pkg_name)
    } else {
        cfg.install_path.clone()
    };
    let dest = contents_dir.join(&install_path);
    if let Some(parent) = dest.parent() { fs::create_dir_all(parent).into_diagnostic()?; }

    match &cfg.source {
        BuildSource::Prebuilt => { pb.set_message("Using prebuilt contents/..."); }

        BuildSource::Download { url, binary_path, strip_components } => {
            let resolved_url = url.replace("{version}", version);
            pb.set_message(format!("Downloading {}...", resolved_url.dimmed()));
            let tmp = tempfile::NamedTempFile::new().into_diagnostic()?;
            let tmp_path = tmp.path().to_str().unwrap().to_string();
            download_file(&resolved_url, &tmp_path)?;

            let is_tar = resolved_url.contains(".tar.") || resolved_url.ends_with(".tgz");
            let is_zip = resolved_url.ends_with(".zip");

            if is_tar {
                let ex = tempfile::tempdir().into_diagnostic()?;
                let mut cmd = std::process::Command::new("tar");
                cmd.arg("-xf").arg(&tmp_path).arg("-C").arg(ex.path());
                if *strip_components > 0 {
                    cmd.arg(format!("--strip-components={}", strip_components));
                }
                if !cmd.status().into_diagnostic()?.success() { bail!("tar extraction failed"); }
                if binary_path.is_empty() { copy_dir_all(ex.path(), &contents_dir)?; }
                else {
                    fs::copy(ex.path().join(binary_path), &dest).into_diagnostic()?;
                    make_executable(&dest)?;
                }
            } else if is_zip {
                let ex = tempfile::tempdir().into_diagnostic()?;
                if !std::process::Command::new("unzip")
                    .args(["-q", &tmp_path, "-d", ex.path().to_str().unwrap()])
                    .status().into_diagnostic()?.success() { bail!("unzip failed"); }
                    if binary_path.is_empty() { copy_dir_all(ex.path(), &contents_dir)?; }
                    else {
                        fs::copy(ex.path().join(binary_path), &dest).into_diagnostic()?;
                        make_executable(&dest)?;
                    }
            } else {
                fs::copy(&tmp_path, &dest).into_diagnostic()?;
                make_executable(&dest)?;
            }
        }

        BuildSource::Build { commands, output } => {
            pb.set_message("Building from source...");
            for (k, v) in &cfg.env { std::env::set_var(k, v); }
            let script = src_dir.join("_hpm_build.sh");
            fs::write(&script, format!("#!/bin/sh\nset -e\n{}", commands.join("\n")))
            .into_diagnostic()?;
            make_executable(&script)?;
            crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest,
                                         &["./_hpm_build.sh".to_string()])?;
                                         let _ = fs::remove_file(&script);
                                         let out = src_dir.join(output);
                                         if !out.exists() {
                                             bail!("Build output '{}' not found. Check build.toml 'output' field.", output);
                                         }
                                         if out.is_dir() { copy_dir_all(&out, &contents_dir)?; }
                                         else { fs::copy(&out, &dest).into_diagnostic()?; make_executable(&dest)?; }
        }
    }
    Ok(contents_dir)
}

// ---------------------------------------------------------------------------
// Version resolution
// ---------------------------------------------------------------------------

fn resolve_version(
    repo: &Repository,
    tags: &git2::string_array::StringArray,
    version: Option<&str>,
    pkg_name: &str,
) -> Result<(String, Oid)> {
    if let Some(v) = version {
        let found = tags.iter().flatten()
        .find(|tag| tag.trim_start_matches('v') == v)
        .ok_or_else(|| miette::miette!("Version {} not found in tags for '{}'.", v, pkg_name))?;
        let obj = repo.revparse_single(found).into_diagnostic()?;
        let commit = obj.peel_to_commit().into_diagnostic()?;
        return Ok((v.to_string(), commit.id()));
    }
    let mut tag_versions: Vec<(String, Oid)> = Vec::new();
    for tag_name in tags.iter().flatten() {
        let ver_str = tag_name.trim_start_matches('v');
        if let Ok(obj) = repo.revparse_single(tag_name) {
            if let Ok(commit) = obj.peel_to_commit() {
                tag_versions.push((ver_str.to_string(), commit.id()));
            }
        }
    }
    if !tag_versions.is_empty() {
        tag_versions.sort_by(|a, b| compare_versions(&a.0, &b.0));
        let (ver, oid) = tag_versions.last().unwrap();
        return Ok((ver.clone(), *oid));
    }
    eprintln!("{} No tags for '{}', installing from HEAD.", "⚠".yellow(), pkg_name);
    let head = repo.head().into_diagnostic()?;
    let commit = head.peel_to_commit().into_diagnostic()?;
    Ok(("HEAD".to_string(), commit.id()))
}
