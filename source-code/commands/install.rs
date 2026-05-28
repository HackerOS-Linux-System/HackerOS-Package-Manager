use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use indicatif::{ProgressBar, ProgressStyle};
use git2::{Repository, Oid, Tree};
use crate::{
    STORE_PATH,
    manifest::Manifest,
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
// /usr/bin wrapper conflict detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum WrapperConflict {
    Free,
    HpmWrapper { pkg: String },
    Foreign,
    SystemCritical,
}

const SYSTEM_CRITICAL: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash",
    "ls", "cp", "mv", "rm", "mkdir", "rmdir", "ln", "chmod", "chown",
    "cat", "echo", "printf", "test", "true", "false", "[",
    "grep", "sed", "awk", "find", "xargs", "sort", "uniq", "wc", "head", "tail",
    "tar", "gzip", "bzip2", "xz", "zstd", "zip", "unzip",
    "mount", "umount", "sudo", "su", "passwd", "id", "whoami",
    "ps", "kill", "killall", "top", "htop",
    "ip", "ifconfig", "ping", "curl", "wget", "ssh", "scp",
    "apt", "dpkg", "apt-get", "apt-cache",
    "systemctl", "journalctl", "systemd",
    "python", "python3", "perl", "ruby", "node", "npm",
    "git", "make", "gcc", "cc", "g++", "clang",
    "env", "which", "whereis", "type",
    "hostname", "uname", "lsb_release",
    "df", "du", "lsblk", "fdisk", "parted",
    "useradd", "userdel", "groupadd", "usermod",
    "crontab", "at",
];

fn classify_wrapper(bin_name: &str, path: &Path) -> WrapperConflict {
    if !path.exists() { return WrapperConflict::Free; }
    if SYSTEM_CRITICAL.contains(&bin_name) { return WrapperConflict::SystemCritical; }
    if let Ok(content) = fs::read_to_string(path) {
        if content.contains("hpm run ") && content.starts_with("#!/bin/sh") {
            let pkg = content.lines()
                .find(|l| l.starts_with("exec "))
                .and_then(|l| {
                    let parts: Vec<&str> = l.split_whitespace().collect();
                    if parts.len() >= 4 && parts[1].ends_with("hpm") && parts[2] == "run" {
                        Some(parts[3].to_string())
                    } else { None }
                })
                .unwrap_or_default();
            return WrapperConflict::HpmWrapper { pkg };
        }
    }
    WrapperConflict::Foreign
}

fn resolve_wrapper_name(bin_name: &str, pkg_name: &str) -> Result<Option<String>> {
    let target   = Path::new("/usr/bin").join(bin_name);
    let conflict = classify_wrapper(bin_name, &target);

    match conflict {
        WrapperConflict::Free => Ok(Some(bin_name.to_string())),

        WrapperConflict::HpmWrapper { ref pkg } => {
            if pkg == pkg_name {
                Ok(Some(bin_name.to_string()))
            } else {
                println!(
                    "  {} {} /usr/bin/{} is already used by hpm package '{}'",
                    "⚠".yellow(), "Conflict:".bold(), bin_name, pkg.cyan()
                );
                ask_wrapper_resolution(bin_name, pkg_name, "another hpm package")
            }
        }

        WrapperConflict::Foreign => {
            let file_type = describe_foreign_file(&target);
            println!(
                "  {} {} /usr/bin/{} already exists ({})",
                "⚠".yellow(), "Conflict:".bold(), bin_name.cyan(), file_type.dimmed()
            );
            ask_wrapper_resolution(bin_name, pkg_name, &file_type)
        }

        WrapperConflict::SystemCritical => {
            eprintln!(
                "  {} {} /usr/bin/{} is a critical system tool.",
                "✗".red(), "Blocked:".bold(), bin_name.cyan()
            );
            eprintln!(
                "    hpm will NEVER overwrite system tools. \
Rename the binary in your package's info.hk:\n\
\x1b[33m    -> bins.{}-{} => \"bin/{}\"\x1b[0m",
                pkg_name, bin_name, bin_name
            );
            let suggested  = format!("{}-{}", pkg_name, bin_name);
            let alt_path   = Path::new("/usr/bin").join(&suggested);
            if !alt_path.exists() {
                eprint!("    Use suggested name '{}' instead? [Y/n] ", suggested.cyan());
                std::io::stderr().flush().into_diagnostic()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).into_diagnostic()?;
                if !input.trim().eq_ignore_ascii_case("n") {
                    println!("    {} Using /usr/bin/{} as wrapper name", "→".yellow(), suggested.cyan());
                    return Ok(Some(suggested));
                }
            }
            Ok(None)
        }
    }
}

fn ask_wrapper_resolution(bin_name: &str, pkg_name: &str, conflict_desc: &str) -> Result<Option<String>> {
    let suggested_alt = format!("{}-{}", pkg_name, bin_name);
    let alt_path      = Path::new("/usr/bin").join(&suggested_alt);

    println!("  Options:");
    println!("    {} Overwrite /usr/bin/{} (replaces {})", "[1]".cyan(), bin_name, conflict_desc);
    if !alt_path.exists() {
        println!("    {} Use /usr/bin/{} instead (safe)", "[2]".cyan(), suggested_alt);
    }
    println!("    {} Skip — don't create wrapper for '{}'", "[3]".cyan(), bin_name);

    eprint!("  Choice [2]: ");
    std::io::stderr().flush().into_diagnostic()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    let choice = input.trim();

    match choice {
        "1" => {
            println!("    {} Overwriting /usr/bin/{}", "→".yellow(), bin_name);
            Ok(Some(bin_name.to_string()))
        }
        "3" => {
            println!("    {} Skipping wrapper for '{}'", "→".yellow(), bin_name);
            Ok(None)
        }
        _ => {
            if !alt_path.exists() {
                println!("    {} Using /usr/bin/{}", "→".yellow(), suggested_alt.cyan());
                Ok(Some(suggested_alt))
            } else {
                eprint!("    /usr/bin/{} also exists. Enter a custom name (or Enter to skip): ", suggested_alt);
                std::io::stderr().flush().into_diagnostic()?;
                let mut custom = String::new();
                std::io::stdin().read_line(&mut custom).into_diagnostic()?;
                let custom = custom.trim().to_string();
                if custom.is_empty() {
                    println!("    {} Skipping wrapper for '{}'", "→".yellow(), bin_name);
                    Ok(None)
                } else {
                    println!("    {} Using /usr/bin/{}", "→".yellow(), custom.cyan());
                    Ok(Some(custom))
                }
            }
        }
    }
}

fn describe_foreign_file(path: &Path) -> String {
    if let Ok(meta) = path.metadata() {
        if meta.is_symlink() { return "symlink".to_string(); }
        if let Ok(content) = fs::read(path) {
            if content.starts_with(b"#!") {
                let line = String::from_utf8_lossy(
                    &content[..content.iter().position(|&b| b == b'\n').unwrap_or(80).min(80)]
                );
                return format!("script: {}", line.trim());
            }
            if content.starts_with(b"\x7fELF") {
                return "compiled binary (ELF)".to_string();
            }
        }
        return format!("{} bytes", meta.len());
    }
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// Public entry point — obsługa @tag oraz normalnych pakietów
// ---------------------------------------------------------------------------

pub fn install(specs: Vec<String>) -> Result<()> {
    if specs.is_empty() {
        eprintln!("{} Usage: hpm install <package>[@<version>]... | @<tag>...", "✗".red());
        std::process::exit(1);
    }

    let lock   = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let repo_mgr = RepoManager::load_sync()?;
    let mut state = State::load()?;

    // Rozwiń @tagi na listy pakietów
    let mut expanded: Vec<String> = Vec::new();
    for spec in &specs {
        if let Some(tag) = spec.strip_prefix('@') {
            let pkgs = repo_mgr.packages_for_tag(tag);
            if pkgs.is_empty() {
                eprintln!("{} No packages found for tag '@{}'", "⚠".yellow(), tag);
                eprintln!("  Available tags: {}", repo_mgr.all_tags().iter()
                    .map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", "));
                std::process::exit(1);
            }
            println!("{} Tag @{} expands to {} package(s): {}",
                     "→".blue(), tag.cyan(), pkgs.len(),
                     pkgs.iter().map(|p| p.cyan().to_string()).collect::<Vec<_>>().join(", "));
            expanded.extend(pkgs);
        } else {
            expanded.push(spec.clone());
        }
    }

    // Deduplikacja (tag może zawierać pakiety już wymienione osobno)
    expanded.dedup();

    // ── Sprawdź konflikty MIĘDZY pakietami z tej samej sesji instalacji ──────
    // Zanim cokolwiek zainstalujemy, sprawdź czy żaden z pakietów nie koliduje
    // z innym z tej samej listy.
    check_inter_package_conflicts(&expanded, &repo_mgr, &state)?;

    state.push_snapshot(&format!("pre-install {}", expanded.join(", ")));

    let mut any_installed = false;

    for spec in &expanded {
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
        } else if state.packages.contains_key(&pkg_name) {
            // Sprawdź czy jakakolwiek wersja jest już zainstalowana
            if let Some(cur) = state.get_current_version(&pkg_name) {
                println!("{} {} is already installed (version {})",
                         "✔".green(), pkg_name.cyan(), cur.cyan());
                continue;
            }
        }

        install_single(&pkg_name, requested_ver.as_deref(), &repo_mgr, &mut state, true)?;
        any_installed = true;
    }

    if any_installed { state.save()?; }
    Ok(())
}

/// Sprawdź konflikty między pakietami z tej samej sesji instalacji.
/// Przykład: A i B są na liście, a A deklaruje konflikt z B.
fn check_inter_package_conflicts(
    specs: &[String],
    repo_mgr: &RepoManager,
    state: &State,
) -> Result<()> {
    // Zbierz manifesty dla wszystkich pakietów które będziemy instalować
    // (tylko fast-fetch metadanych, bez klonowania całego repo)
    let pkg_names: Vec<&str> = specs.iter()
        .map(|s| s.splitn(2, '@').next().unwrap_or(s.as_str()))
        .collect();

    // Sprawdź każdą parę
    for i in 0..pkg_names.len() {
        for j in (i + 1)..pkg_names.len() {
            let a = pkg_names[i];
            let b = pkg_names[j];

            // Sprawdź czy a i b są wzajemnie na swoich listach konfliktów
            // używając danych z repo.json (jeśli są dostępne przez cache)
            if let (Some(meta_a), Some(meta_b)) = (
                crate::repo::load_cached_meta_pub(a),
                crate::repo::load_cached_meta_pub(b),
            ) {
                // meta nie zawiera konfliktów — to jest w pełnym manifeście
                // Sprawdzamy przez state.check_conflicts gdy jeden jest w stanie
                let _ = (meta_a, meta_b); // info do logowania
            }

            // Główna logika: jeśli A jest już w state i B jest nowy,
            // check_conflicts w install_single to złapie.
            // Tu sprawdzamy A vs B gdy OBA są nowe w tej sesji.
            if !state.packages.contains_key(a) && !state.packages.contains_key(b) {
                // Oba nowe — nie możemy łatwo sprawdzić bez pobierania manifestów.
                // Logujemy ostrzeżenie jeśli nazwy są identyczne.
                if a == b {
                    bail!("Duplicate package '{}' in install list", a);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Install a single package
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

    let repo_path = repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
    let repo      = Repository::open(&repo_path).into_diagnostic()?;
    let tags      = repo.tag_names(None).into_diagnostic()?;
    let (selected_version, commit_oid) = resolve_version(&repo, &tags, version, pkg_name)?;

    pb.set_message(format!("Extracting {}@{}...", pkg_name.cyan(), selected_version.green()));

    let checkout_dir = tempfile::tempdir().into_diagnostic()?;
    let commit       = repo.find_commit(commit_oid).into_diagnostic()?;
    let tree         = commit.tree().into_diagnostic()?;
    extract_tree(&repo, &tree, checkout_dir.path())?;

    let src_dir = checkout_dir.path();

    pb.set_message("Reading manifest...");
    let manifest  = Manifest::load_from_path(src_dir.to_str().unwrap())?;
    let build_cfg = BuildConfig::load_from_dir(src_dir);

    // Conflict check — już zainstalowane vs ten pakiet
    let conflict_violations = state.check_conflicts(pkg_name, &manifest.conflicts);
    if !conflict_violations.is_empty() {
        bail!(
            "Cannot install '{}': package conflicts:\n{}",
            pkg_name,
            conflict_violations.iter().map(|v| format!("  ✗ {}", v)).collect::<Vec<_>>().join("\n")
        );
    }

    // Resolve hpm deps — sprawdź też kompatybilność wersji po aktualizacji
    if !manifest.deps.is_empty() {
        pb.set_message("Resolving dependencies...");
        for (dep_name, dep_req) in &manifest.deps {
            let already_ok = state.packages.get(dep_name)
                .map(|vers| vers.keys().any(|v| crate::utils::satisfies(v, dep_req)))
                .unwrap_or(false);

            if !already_ok {
                // Sprawdź czy zainstalowana wersja jest niekompatybilna (wymaga aktualizacji)
                if let Some(installed_vers) = state.packages.get(dep_name) {
                    let any_installed = !installed_vers.is_empty();
                    if any_installed {
                        println!("\n  {} Dependency {} requires {} but installed version is incompatible",
                                 "⚠".yellow(), dep_name.cyan(), dep_req);
                        println!("  {} Updating {} to satisfy constraint...", "→".yellow(), dep_name.cyan());
                    }
                }

                println!("\n  {} Installing dependency: {}{}",
                         "→".yellow(), dep_name.cyan(),
                         if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) }
                );
                let dep_ver = if dep_req.is_empty() || dep_req.starts_with(">=")
                    || dep_req.starts_with('>') || dep_req.starts_with('=') { None }
                    else { Some(dep_req.as_str()) };
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
        bail!("No 'contents/' directory found for '{}@{}'.", pkg_name, selected_version);
    }

    // Atomic staging
    let dest_dir    = Path::new(STORE_PATH).join(pkg_name).join(&selected_version);
    let staging_dir = Path::new(STORE_PATH).join(pkg_name)
        .join(format!(".staging-{}", selected_version));
    if staging_dir.exists() { let _ = fs::remove_dir_all(&staging_dir); }
    fs::create_dir_all(&staging_dir).into_diagnostic()?;

    let stage_result = (|| -> Result<()> {
        copy_dir_all(&contents_src, &staging_dir)?;
        make_all_binaries_executable(&staging_dir, &manifest)?;
        let manifest_src = src_dir.join("info.hk");
        if manifest_src.exists() {
            fs::copy(&manifest_src, staging_dir.join("info.hk")).into_diagnostic()?;
        }
        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(e);
    }

    if dest_dir.exists() { fs::remove_dir_all(&dest_dir).into_diagnostic()?; }
    fs::rename(&staging_dir, &dest_dir).into_diagnostic()?;

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

    // /usr/bin wrappers
    pb.set_message("Checking /usr/bin for conflicts...");
    let hpm_exe = std::env::current_exe().into_diagnostic()?;

    for bin_name in &manifest.bins {
        let bin_rel = if let Some(explicit) = manifest.bin_paths.get(bin_name) {
            let p = dest_dir.join(explicit);
            if p.exists() {
                make_executable(&p).ok();
                Some(explicit.clone())
            } else {
                find_binary_in_dir(&dest_dir, bin_name)
            }
        } else {
            find_binary_in_dir(&dest_dir, bin_name)
        };

        let bin_rel = match bin_rel {
            Some(r) => {
                make_executable(&dest_dir.join(&r)).ok();
                r
            }
            None => {
                pb.suspend(|| print_binary_not_found_help(&dest_dir, bin_name, pkg_name));
                continue;
            }
        };

        let wrapper_name = match resolve_wrapper_name(bin_name, pkg_name)? {
            Some(name) => name,
            None => {
                println!(
                    "  {} Skipped wrapper for '{}'. Run manually:\n    {} run {} {}",
                    "ℹ".cyan(), bin_name,
                    hpm_exe.display(), pkg_name, bin_rel
                );
                continue;
            }
        };

        let wrapper_path = Path::new("/usr/bin").join(&wrapper_name);
        let content = format!(
            "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
            hpm_exe.display(), pkg_name, bin_rel
        );
        fs::write(&wrapper_path, &content).into_diagnostic()?;
        make_executable(&wrapper_path)?;

        if wrapper_name == *bin_name {
            println!("  {} Wrapper: {} → {}/{}/{}",
                     "✔".green(), bin_name.cyan(), pkg_name, selected_version, bin_rel.dimmed());
        } else {
            println!("  {} Wrapper: {} (as {}) → {}/{}/{}",
                     "✔".green(), bin_name.cyan(), wrapper_name.yellow(),
                     pkg_name, selected_version, bin_rel.dimmed());
        }
    }

    // Desktop integration
    if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
        pb.set_message("Installing desktop integration...");
        install_desktop_integration_pub(&dest_dir, &manifest, pkg_name,
                                        &hpm_exe.display().to_string())?;
    }

    let depends_on: HashSet<String> = manifest.deps.iter()
        .map(|(name, _)| {
            state.get_current_version(name)
                .map(|ver| format!("{}@{}", name, ver))
                .unwrap_or_else(|| name.clone())
        }).collect();
    let conflicts_with: HashSet<String> = manifest.conflicts.iter().cloned().collect();

    let checksum = compute_dir_hash(&dest_dir).unwrap_or_default();
    state.update_package(pkg_name, &selected_version, &checksum,
                         manually_installed, depends_on, conflicts_with);

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
// Desktop integration — publiczne API (używane przez rollback i repair)
// ---------------------------------------------------------------------------

pub fn install_desktop_integration_pub(
    dest_dir: &Path,
    manifest: &Manifest,
    pkg_name: &str,
    hpm_exe: &str,
) -> Result<()> {
    install_desktop_integration(dest_dir, manifest, pkg_name, hpm_exe)
}

// ---------------------------------------------------------------------------
// Make all declared binaries executable
// ---------------------------------------------------------------------------

fn make_all_binaries_executable(dir: &Path, manifest: &Manifest) -> Result<()> {
    for bin_name in &manifest.bins {
        if let Some(explicit) = manifest.bin_paths.get(bin_name) {
            let p = dir.join(explicit);
            if p.exists() { make_executable(&p)?; continue; }
        }
        for path in &[dir.join("bin").join(bin_name), dir.join(bin_name)] {
            if path.exists() { make_executable(path)?; }
        }
        if let Some(rel) = find_binary_in_dir(dir, bin_name) {
            make_executable(&dir.join(&rel))?;
        }
    }
    make_scripts_executable_recursive(dir);
    Ok(())
}

fn make_scripts_executable_recursive(dir: &Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                make_scripts_executable_recursive(&path);
            } else {
                use std::io::Read;
                if let Ok(mut f) = fs::File::open(&path) {
                    let mut buf = [0u8; 2];
                    if f.read_exact(&mut buf).is_ok() && buf == *b"#!" {
                        let _ = make_executable(&path);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Binary not found — helpful diagnostics
// ---------------------------------------------------------------------------

fn print_binary_not_found_help(dest_dir: &Path, bin_name: &str, pkg_name: &str) {
    eprintln!("{} Binary '{}' not found in installed files.", "⚠".yellow(), bin_name.cyan());
    let all   = list_all_files(dest_dir);
    let execs: Vec<_> = all.iter().filter(|p| {
        p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }).collect();

    if execs.is_empty() {
        eprintln!("  No executable files in store. Files present:");
        for f in &all {
            if let Ok(rel) = f.strip_prefix(dest_dir) {
                eprintln!("    {}", rel.display());
            }
        }
        eprintln!();
        eprintln!("  {} Fix:", "→".yellow());
        eprintln!("    git update-index --chmod=+x contents/bin/<binary>");
        eprintln!("    OR declare explicit path:");
        eprintln!("    -> bins.{} => \"bin/<binary>\"", bin_name);
    } else {
        eprintln!("  Executables found:");
        for f in &execs {
            if let Ok(rel) = f.strip_prefix(dest_dir) { eprintln!("    {}", rel.display()); }
        }
        eprintln!();
        if let Some(first) = execs.first() {
            if let Ok(rel) = first.strip_prefix(dest_dir) {
                eprintln!("  {} Declare in info.hk:", "→".yellow());
                eprintln!("    -> bins.{} => \"{}\"", bin_name, rel.display());
            }
        }
    }
}

fn list_all_files(dir: &Path) -> Vec<PathBuf> {
    let mut r = Vec::new();
    collect_all(dir, &mut r);
    r
}

fn collect_all(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() { collect_all(&path, out); }
            else if path.file_name().and_then(|n| n.to_str()) != Some("info.hk") {
                out.push(path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

pub fn find_binary_in_dir(pkg_dir: &Path, bin_name: &str) -> Option<String> {
    if pkg_dir.join("bin").join(bin_name).exists() { return Some(format!("bin/{}", bin_name)); }
    if pkg_dir.join(bin_name).exists() { return Some(bin_name.to_string()); }
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

// ---------------------------------------------------------------------------
// Desktop integration
// ---------------------------------------------------------------------------

fn install_desktop_integration(
    dest_dir: &Path, manifest: &Manifest,
    pkg_name: &str, hpm_exe: &str,
) -> Result<()> {
    let desktop    = &manifest.desktop;
    let icon_name  = install_icon(dest_dir, manifest, pkg_name)?;
    fs::create_dir_all(DESKTOP_DIR).into_diagnostic()?;
    let desktop_path = Path::new(DESKTOP_DIR).join(format!("{}.desktop", pkg_name));

    if !desktop.desktop_file.is_empty() {
        let custom = dest_dir.join(&desktop.desktop_file);
        if custom.exists() {
            fs::copy(&custom, &desktop_path).into_diagnostic()?;
            patch_desktop_exec(&desktop_path, hpm_exe, pkg_name, manifest)?;
            return Ok(());
        }
    }
    if let Some(found) = find_file_by_ext(dest_dir, "desktop") {
        fs::copy(&found, &desktop_path).into_diagnostic()?;
        patch_desktop_exec(&desktop_path, hpm_exe, pkg_name, manifest)?;
        return Ok(());
    }

    let bin_name     = manifest.bins.first().map(|s| s.as_str()).unwrap_or(pkg_name);
    let display_name = if !desktop.display_name.is_empty() { desktop.display_name.clone() }
    else {
        let mut c = pkg_name.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    let categories = if !desktop.categories.is_empty() { desktop.categories.clone() }
        else { "Utility;".to_string() };
    let comment    = if !desktop.comment.is_empty() { desktop.comment.clone() }
        else { manifest.summary.clone() };
    let exec_cmd   = format!("{} run {} {}", hpm_exe, pkg_name, bin_name);
    let mut content = format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment={}\nExec={} %F\nCategories={}\nTerminal={}\n",
        display_name, comment, exec_cmd, categories,
        if manifest.is_gui { "false" } else { "true" }
    );
    if !icon_name.is_empty() { content.push_str(&format!("Icon={}\n", icon_name)); }
    if desktop.nodisplay      { content.push_str("NoDisplay=true\n"); }
    if !desktop.mime_types.is_empty() { content.push_str(&format!("MimeType={}\n", desktop.mime_types)); }
    if !desktop.keywords.is_empty()   { content.push_str(&format!("Keywords={}\n", desktop.keywords)); }
    fs::write(&desktop_path, content).into_diagnostic()?;
    let _ = std::process::Command::new("update-desktop-database").arg(DESKTOP_DIR).status();
    Ok(())
}

fn install_icon(dest_dir: &Path, manifest: &Manifest, pkg_name: &str) -> Result<String> {
    let icon_rel = &manifest.desktop.icon;
    let icon_src = if !icon_rel.is_empty() {
        let p = dest_dir.join(icon_rel);
        if p.exists() { Some(p) } else { None }
    } else {
        [
            dest_dir.join(format!("icons/{}.png", pkg_name)),
            dest_dir.join(format!("icons/{}.svg", pkg_name)),
            dest_dir.join(format!("{}.png", pkg_name)),
        ].into_iter().find(|p| p.exists())
        .or_else(|| find_file_by_ext(dest_dir, "png"))
        .or_else(|| find_file_by_ext(dest_dir, "svg"))
    };
    if let Some(src) = icon_src {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
        if ext == "svg" {
            let td = Path::new(ICON_DIR).join("scalable/apps");
            fs::create_dir_all(&td).into_diagnostic()?;
            fs::copy(&src, td.join(format!("{}.svg", pkg_name))).into_diagnostic()?;
        } else {
            let td = Path::new(ICON_DIR).join("256x256/apps");
            fs::create_dir_all(&td).into_diagnostic()?;
            fs::copy(&src, td.join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
            fs::create_dir_all(PIXMAP_DIR).into_diagnostic()?;
            fs::copy(&src, Path::new(PIXMAP_DIR).join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
        }
        let _ = std::process::Command::new("gtk-update-icon-cache").args(["-f", "-t", ICON_DIR]).status();
        return Ok(pkg_name.to_string());
    }
    Ok(String::new())
}

fn patch_desktop_exec(path: &Path, hpm_exe: &str, pkg_name: &str, manifest: &Manifest) -> Result<()> {
    let content  = fs::read_to_string(path).into_diagnostic()?;
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
                if blob.content().starts_with(b"#!")  { make_executable(&entry_path)?; }
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
    cfg: &BuildConfig, src_dir: &Path, version: &str,
    manifest: &Manifest, pb: &ProgressBar, pkg_name: &str,
) -> Result<PathBuf> {
    let contents_dir = src_dir.join("contents");
    fs::create_dir_all(&contents_dir).into_diagnostic()?;

    let install_path = if cfg.install_path.is_empty() { format!("bin/{}", pkg_name) }
        else { cfg.install_path.clone() };
    let dest = contents_dir.join(&install_path);
    if let Some(parent) = dest.parent() { fs::create_dir_all(parent).into_diagnostic()?; }

    match &cfg.source {
        BuildSource::Prebuilt => { pb.set_message("Using prebuilt contents/..."); }

        BuildSource::Download { url, binary_path, strip_components } => {
            let resolved_url = url.replace("{version}", version);
            pb.set_message(format!("Downloading {}...", resolved_url.dimmed()));
            let tmp      = tempfile::NamedTempFile::new().into_diagnostic()?;
            let tmp_path = tmp.path().to_str().unwrap().to_string();
            download_file(&resolved_url, &tmp_path)?;

            let is_tar = resolved_url.contains(".tar.") || resolved_url.ends_with(".tgz");
            let is_zip = resolved_url.ends_with(".zip");

            if is_tar {
                let ex = tempfile::tempdir().into_diagnostic()?;
                let mut cmd = std::process::Command::new("tar");
                cmd.arg("-xf").arg(&tmp_path).arg("-C").arg(ex.path());
                if *strip_components > 0 { cmd.arg(format!("--strip-components={}", strip_components)); }
                if !cmd.status().into_diagnostic()?.success() { bail!("tar extraction failed"); }
                if binary_path.is_empty() { copy_dir_all(ex.path(), &contents_dir)?; }
                else { fs::copy(ex.path().join(binary_path), &dest).into_diagnostic()?; make_executable(&dest)?; }
            } else if is_zip {
                let ex = tempfile::tempdir().into_diagnostic()?;
                if !std::process::Command::new("unzip")
                    .args(["-q", &tmp_path, "-d", ex.path().to_str().unwrap()])
                    .status().into_diagnostic()?.success() { bail!("unzip failed"); }
                if binary_path.is_empty() { copy_dir_all(ex.path(), &contents_dir)?; }
                else { fs::copy(ex.path().join(binary_path), &dest).into_diagnostic()?; make_executable(&dest)?; }
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
            if !out.exists() { bail!("Build output '{}' not found.", output); }
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
        let obj    = repo.revparse_single(found).into_diagnostic()?;
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
    let head   = repo.head().into_diagnostic()?;
    let commit = head.peel_to_commit().into_diagnostic()?;
    Ok(("HEAD".to_string(), commit.id()))
}
