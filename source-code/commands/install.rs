use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use indicatif::{ProgressBar, ProgressStyle};
use git2::{Repository, Oid, Tree};
use crate::{
    manifest::Manifest,
    repo::{RepoManager, BuildConfig, BuildSource},
    state::{State, WrapperNames, split_pkg_ver},
    utils::{
        acquire_lock, release_lock, compute_dir_hash, copy_dir_all,
        make_executable, compare_versions, download_file,
    },
};


// ---------------------------------------------------------------------------
// SIGINT / SIGTERM cleanup registry
// ---------------------------------------------------------------------------

/// Globalna lista ścieżek staging które mają być wyczyszczone przy SIGINT.
static STAGING_REGISTRY: std::sync::OnceLock<Arc<Mutex<Vec<PathBuf>>>> = std::sync::OnceLock::new();

fn staging_registry() -> &'static Arc<Mutex<Vec<PathBuf>>> {
    STAGING_REGISTRY.get_or_init(|| {
        let registry = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let reg_clone = Arc::clone(&registry);

        // Zarejestruj handler SIGINT i SIGTERM
        unsafe {
            libc::signal(libc::SIGINT, sigint_handler as libc::sighandler_t);
            libc::signal(libc::SIGTERM, sigint_handler as libc::sighandler_t);
        }

        registry
    })
}

extern "C" fn sigint_handler(_sig: libc::c_int) {
    // Wyczyść wszystkie staging dirs
    if let Some(registry) = STAGING_REGISTRY.get() {
        if let Ok(dirs) = registry.lock() {
            for dir in dirs.iter() {
                if dir.exists() {
                    let _ = fs::remove_dir_all(dir);
                    eprintln!("\nhpm: cleaned up staging: {}", dir.display());
                }
            }
        }
    }
    // Przywróć domyślny handler i ponownie wyślij sygnał
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

fn register_staging(path: &Path) {
    if let Ok(mut dirs) = staging_registry().lock() {
        dirs.push(path.to_owned());
    }
}

fn unregister_staging(path: &Path) {
    if let Ok(mut dirs) = staging_registry().lock() {
        dirs.retain(|p| p != path);
    }
}

// ---------------------------------------------------------------------------
// Wrapper conflict detection
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
    "systemctl", "journalctl",
    "python", "python3", "perl", "ruby", "node", "npm",
    "git", "make", "gcc", "cc", "g++", "clang",
    "env", "which", "whereis", "type",
    "hostname", "uname", "df", "du",
    "useradd", "userdel", "groupadd", "usermod",
    "crontab",
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
    // Sprawdź persystentny cache wyborów
    let wn = WrapperNames::load();
    if let Some(cached_name) = wn.get(pkg_name, bin_name) {
        let cached_path = Path::new(crate::bin_dir()).join(cached_name);
        let conflict    = classify_wrapper(cached_name, &cached_path);
        if conflict == WrapperConflict::Free
            || conflict == (WrapperConflict::HpmWrapper { pkg: pkg_name.to_string() })
        {
            return Ok(Some(cached_name.to_string()));
        }
    }

    let target   = Path::new(crate::bin_dir()).join(bin_name);
    let conflict = classify_wrapper(bin_name, &target);

    let result = match conflict {
        WrapperConflict::Free => Ok(Some(bin_name.to_string())),

        WrapperConflict::HpmWrapper { ref pkg } => {
            if pkg == pkg_name { Ok(Some(bin_name.to_string())) }
            else {
                println!("  {} {} {}/{} used by '{}'",
                         "⚠".bright_black(), "Conflict:".bold(), crate::bin_dir().dimmed(), bin_name, pkg.white());
                ask_wrapper_resolution(bin_name, pkg_name, "another hpm package")
            }
        }

        WrapperConflict::Foreign => {
            let ft = describe_foreign_file(&target);
            println!("  {} {} {}/{} exists ({})",
                     "⚠".bright_black(), "Conflict:".bold(), crate::bin_dir().dimmed(), bin_name.white(), ft.dimmed());
            ask_wrapper_resolution(bin_name, pkg_name, &ft)
        }

        WrapperConflict::SystemCritical => {
            eprintln!("  {} {}/{} is a critical system tool — blocked.",
                      "✗".red(), crate::bin_dir().dimmed(), bin_name.white());
            let suggested = format!("{}-{}", pkg_name, bin_name);
            let alt_path  = Path::new(crate::bin_dir()).join(&suggested);
            if !alt_path.exists() {
                eprint!("    Use '{}' instead? [Y/n] ", suggested.white());
                std::io::stderr().flush().into_diagnostic()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).into_diagnostic()?;
                if !input.trim().eq_ignore_ascii_case("n") {
                    return Ok(Some(suggested));
                }
            }
            Ok(None)
        }
    };

    // Zapisz niestandardowy wybór
    if let Ok(Some(ref chosen)) = result {
        if chosen != bin_name {
            let mut wn = WrapperNames::load();
            wn.set(pkg_name, bin_name, chosen);
        }
    }
    result
}

fn ask_wrapper_resolution(bin_name: &str, pkg_name: &str, conflict_desc: &str) -> Result<Option<String>> {
    let suggested = format!("{}-{}", pkg_name, bin_name);
    let alt_path  = Path::new(crate::bin_dir()).join(&suggested);
    println!("  [1] Overwrite {}/{} (replaces {})", crate::bin_dir(), bin_name, conflict_desc);
    if !alt_path.exists() { println!("  [2] Use {}/{} instead (safe)", crate::bin_dir(), suggested); }
    println!("  [3] Skip wrapper for '{}'", bin_name);
    eprint!("  Choice [2]: ");
    std::io::stderr().flush().into_diagnostic()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    match input.trim() {
        "1" => Ok(Some(bin_name.to_string())),
        "3" => Ok(None),
        _   => {
            if !alt_path.exists() { Ok(Some(suggested)) }
            else {
                eprint!("    {}/{} also exists. Custom name (or Enter to skip): ", crate::bin_dir(), suggested);
                std::io::stderr().flush().into_diagnostic()?;
                let mut custom = String::new();
                std::io::stdin().read_line(&mut custom).into_diagnostic()?;
                let custom = custom.trim().to_string();
                if custom.is_empty() { Ok(None) } else { Ok(Some(custom)) }
            }
        }
    }
}

fn describe_foreign_file(path: &Path) -> String {
    if let Ok(meta) = path.metadata() {
        if meta.is_symlink() { return "symlink".to_string(); }
        if let Ok(content) = fs::read(path) {
            if content.starts_with(b"#!") {
                let end = content.iter().position(|&b| b == b'\n').unwrap_or(80).min(80);
                return format!("script: {}", String::from_utf8_lossy(&content[..end]).trim());
            }
            if content.starts_with(b"\x7fELF") { return "compiled binary (ELF)".to_string(); }
        }
        return format!("{} bytes", meta.len());
    }
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// ATOMOWY zapis wrappera — NOWE
// Zamiast fs::write() bezpośrednio na <bin_dir>/<name> piszemy do .tmp i rename().
// Jeśli hpm zginie w połowie — tmp zostaje, ale <bin_dir>/<name> jest albo stary albo nowy.
// ---------------------------------------------------------------------------

fn write_wrapper_atomic(wrapper_path: &Path, content: &str) -> Result<()> {
    let tmp_path = wrapper_path.with_extension("hpm.tmp");
    // Zapis do pliku tymczasowego
    fs::write(&tmp_path, content.as_bytes()).into_diagnostic()?;
    make_executable(&tmp_path)?;
    // Atomowe zastąpienie (rename jest atomowe w obrębie tego samego filesystemu)
    fs::rename(&tmp_path, wrapper_path).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn install(specs: Vec<String>) -> Result<()> {
    // --release: zamiast `git clone` całego repo, pobierz gotowe archiwum
    // .hpm dołączone do GitHub Release (patrz `install_single_from_release`
    // i `hpm build`). repo.json/repo-list.json wygląda identycznie jak
    // zawsze — to tylko sposób *pobrania* tego samego pakietu.
    let use_release = specs.iter().any(|s| s == "--release");
    // --require-signed: odmów instalacji .hpm bez ważnego podpisu GPG,
    // zamiast tylko ostrzegać. Ma sens tylko razem z --release (git clone
    // nie ma w ogóle koncepcji podpisu na tym etapie).
    let require_signed = specs.iter().any(|s| s == "--require-signed");
    // --verbose/-v: to samo co globalne `hpm --verbose install ...`, tylko
    // podane PO nazwie komendy — dokładnie tak, jak ktoś naturalnie by
    // spróbował (`hpm install cosmic --verbose`). Oba miejsca działają.
    if specs.iter().any(|s| s == "--verbose" || s == "-v") {
        crate::set_verbose(true);
    }
    let specs: Vec<String> = specs.into_iter()
        .filter(|s| s != "--release" && s != "--require-signed" && s != "--verbose" && s != "-v")
        .collect();

    if require_signed && !use_release {
        eprintln!("{} --require-signed only makes sense together with --release", "✗".red());
        std::process::exit(1);
    }

    if specs.is_empty() {
        eprintln!("{} Usage: hpm install <package>[@<version>]... | @<tag>... [--release] [--require-signed] [--verbose]", "✗".red());
        std::process::exit(1);
    }

    let lock      = acquire_lock()?;
    let _guard    = scopeguard::guard(lock, |_| release_lock());
    let repo_mgr  = RepoManager::load_sync()?;
    let mut state = State::load()?;

    // Rozwiń @tagi
    let mut expanded: Vec<String> = Vec::new();
    for spec in &specs {
        if let Some(tag) = spec.strip_prefix('@') {
            let pkgs = repo_mgr.packages_for_tag(tag);
            if pkgs.is_empty() {
                eprintln!("{} No packages for tag '@{}'", "⚠".bright_black(), tag);
                let all = repo_mgr.all_tags();
                if !all.is_empty() {
                    eprintln!("  Available tags: {}",
                        all.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", "));
                }
                std::process::exit(1);
            }
            println!("{} @{} → {} packages: {}",
                     "→".white(), tag.white(), pkgs.len(),
                     pkgs.iter().map(|p| p.white().to_string()).collect::<Vec<_>>().join(", "));
            expanded.extend(pkgs);
        } else {
            expanded.push(spec.clone());
        }
    }
    expanded.dedup();
    check_inter_package_conflicts(&expanded)?;

    // Pomiń pakiety już usatysfakcjonowane: jeśli poproszono o konkretną
    // wersję i jest zainstalowana — gotowe; jeśli nie poproszono o wersję i
    // JAKAŚ wersja jest aktywna — też gotowe (jak poprzednio, zanim solver
    // wszedł do gry — to nie jest coś, co solver powinien przesłaniać).
    let mut to_install: Vec<String> = Vec::new();
    for spec in &expanded {
        let (pkg_name, requested_ver) = split_spec(spec);
        if repo_mgr.get_package_url(&pkg_name).is_none() {
            return Err(miette::miette!(
                "Package '{}' not found.\n  Run {} to refresh.",
                pkg_name, "hpm refresh".bright_black()
            ));
        }
        if let Some(ver) = &requested_ver {
            if state.packages.get(&pkg_name).map(|vs| vs.contains_key(ver.as_str())).unwrap_or(false) {
                println!("{} {}@{} is already installed", "✔".red(), pkg_name.white(), ver.white());
                continue;
            }
        } else if let Some(cur) = state.get_current_version(&pkg_name) {
            println!("{} {} is already installed ({})", "✔".red(), pkg_name.white(), cur.white());
            continue;
        }
        to_install.push(spec.clone());
    }

    if to_install.is_empty() {
        println!("{} Nothing to do.", "✔".red());
        return Ok(());
    }

    let resolved = solve_batch_versions(&to_install, &repo_mgr, &state)?;

    state.push_snapshot(&format!("pre-install {}", to_install.join(", ")));
    let mut any_installed = false;

    for spec in &to_install {
        let (pkg_name, _) = split_spec(spec);
        let selected_ver  = resolved.get(&pkg_name)
            .map(|(v, _)| v.clone())
            .ok_or_else(|| miette::miette!("Internal error: no resolved version for '{}'", pkg_name))?;

        if use_release {
            install_single_from_release(&pkg_name, Some(&selected_ver), &repo_mgr, &mut state, true, require_signed)?;
        } else {
            install_single(&pkg_name, Some(&selected_ver), &repo_mgr, &mut state, true)?;
        }
        any_installed = true;
    }

    if any_installed { state.save()?; }
    Ok(())
}

fn check_inter_package_conflicts(specs: &[String]) -> Result<()> {
    let pkg_names: Vec<&str> = specs.iter()
        .map(|s| { let i = s.find('@').unwrap_or(s.len()); &s[..i] })
        .collect();
    for i in 0..pkg_names.len() {
        for j in (i+1)..pkg_names.len() {
            if pkg_names[i] == pkg_names[j] {
                bail!("Duplicate package '{}' in install list", pkg_names[i]);
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) version:  String,
    pub(crate) manifest: Manifest,
}

/// Prawdziwy (choć celowo ograniczony) solver wersji — backtracking, nie
/// pełny SAT, ale robi dokładnie to, czego brakowało: jeśli najnowsza wersja
/// pakietu A koliduje z pakietem B w tym samym `hpm install`, spróbuj
/// STARSZEJ wersji A (albo B) zamiast po prostu odrzucać całą operację.
///
/// Zasady:
/// - Wersja jawnie przypięta (`pkg@1.2.3`) ma dokładnie JEDNEGO kandydata —
///   solver nigdy nie podstawia czegoś innego niż to, o co jawnie poproszono.
/// - Bez przypięcia: do `MAX_CANDIDATES_PER_PKG` najnowszych tagów, od
///   najnowszego. Solver preferuje najnowsze wersje (przeszukuje w tej
///   kolejności i zwraca PIERWSZE spójne przypisanie).
/// - Ograniczony budżet przeszukiwania (`MAX_SEARCH_STEPS`) żeby nie
///   eksplodować kombinatorycznie na dużych wsadach — po jego wyczerpaniu
///   solver poddaje się z jasnym błędem zamiast wisieć.
const MAX_CANDIDATES_PER_PKG: usize = 6;
const MAX_SEARCH_STEPS:       usize = 2000;

fn solve_batch_versions(
    specs: &[String], repo_mgr: &RepoManager, state: &State,
) -> Result<HashMap<String, (String, Manifest)>> {
    println!("{} Resolving versions for {} package(s)...", "→".white(), specs.len());

    let mut candidates: HashMap<String, Vec<Candidate>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for spec in specs {
        let (pkg_name, requested_ver) = split_spec(spec);
        let pkg_url = repo_mgr.get_package_url(&pkg_name)
            .ok_or_else(|| miette::miette!("Package '{}' not found", pkg_name))?;
        let repo_path = repo_mgr.clone_package_repo(&pkg_name, pkg_url)?;
        let repo      = Repository::open(&repo_path).into_diagnostic()?;
        let tags      = repo.tag_names(None).into_diagnostic()?;

        let mut cands: Vec<Candidate> = Vec::new();
        // Track *why* candidates got rejected so that "no installable
        // version" can point at the real cause instead of being a dead
        // end. Two very different situations used to produce the exact
        // same generic error:
        //   (a) the repo genuinely has zero git tags, vs.
        //   (b) tags exist, but info.hk failed to parse on all of them
        //       (manifest_at_commit's error was silently discarded by
        //       `if let Ok(m) = ... { }` with no `else`).
        // (b) is exactly what a broken info.hk on a freshly-tagged repo
        // looks like, and it deserves to show the actual parse error
        // (already rendered nicely by hk-parser, see manifest.rs) right
        // here at `hpm install` time — not just in `hpm dev`.
        let mut last_manifest_err: Option<miette::Report> = None;
        let total_tag_count = tags.iter().flatten().count();

        if let Some(ver) = &requested_ver {
            let (v, oid) = resolve_version(&repo, &tags, Some(ver.as_str()), &pkg_name)?;
            match manifest_at_commit(&repo, oid) {
                Ok(m) => cands.push(Candidate { version: v, manifest: m }),
                Err(e) => last_manifest_err = Some(e),
            }
        } else {
            let mut tag_versions: Vec<(String, Oid)> = Vec::new();
            for tag_name in tags.iter().flatten() {
                let ver_str = tag_name.trim_start_matches('v');
                if let Ok(obj) = repo.revparse_single(tag_name) {
                    if let Ok(commit) = obj.peel_to_commit() {
                        tag_versions.push((ver_str.to_string(), commit.id()));
                    }
                }
            }
            tag_versions.sort_by(|a, b| compare_versions(&b.0, &a.0)); // newest first
            for (v, oid) in tag_versions.into_iter().take(MAX_CANDIDATES_PER_PKG) {
                match manifest_at_commit(&repo, oid) {
                    Ok(m) => cands.push(Candidate { version: v, manifest: m }),
                    // Keep only the FIRST failure we hit — since tag_versions
                    // is sorted newest-first, that's the newest tag's error,
                    // the one the user most likely wants to see.
                    Err(e) => {
                        if last_manifest_err.is_none() {
                            last_manifest_err = Some(e);
                        }
                    }
                }
            }
        }

        if cands.is_empty() {
            if total_tag_count == 0 {
                bail!(
                    "No installable version found for '{pkg}' — this repository has no git tags.\n  \
                     hpm reads installable versions ONLY from git tags — not from repo.json, not\n  \
                     from a GitHub Release, and not from the default branch. Push one:\n\n  \
                     git tag <version>          (e.g. git tag 1.0.15, matching info.hk's version)\n  \
                     git push origin <version>\n\n  \
                     Then run `hpm refresh` and try again.",
                    pkg = pkg_name
                );
            } else if let Some(e) = last_manifest_err {
                bail!(
                    "No installable version found for '{pkg}' — {n} tag(s) exist, but info.hk \
                     failed to load on every one of them. Most recent failure:\n\n{err}",
                    pkg = pkg_name, n = total_tag_count, err = e
                );
            } else {
                bail!(
                    "No installable version found for '{}' — {} tag(s) exist but none could be \
                     read (corrupt commit, missing info.hk in the tagged tree, or similar).",
                    pkg_name, total_tag_count
                );
            }
        }
        order.push(pkg_name.clone());
        candidates.insert(pkg_name, cands);
    }

    let mut assignment: HashMap<String, (String, Manifest)> = HashMap::new();
    let mut steps = MAX_SEARCH_STEPS;
    if !backtrack_solve(&order, 0, &candidates, state, &mut assignment, &mut steps) {
        let details: Vec<String> = order.iter().map(|n| {
            let vers: Vec<&str> = candidates[n].iter().map(|c| c.version.as_str()).collect();
            format!("  {} — tried: {}", n, vers.join(", "))
        }).collect();
        bail!(
            "Could not find a conflict-free combination of versions for this batch:\n{}\n\
  Try installing these one at a time, or pin exact versions with '{}'.",
            details.join("\n"), "pkg@version".bright_black()
        );
    }

    for (name, (ver, _)) in &assignment {
        let requested = specs.iter().find(|s| split_spec(s).0 == *name)
            .map(|s| split_spec(s).1).flatten();
        if requested.is_none() {
            // Poinformuj tylko gdy solver faktycznie WYBRAŁ coś (nie tylko
            // spełnił jawne przypięcie) — żeby nie zaśmiecać wyjścia dla
            // zwykłego, bezkonfliktowego przypadku z jedną najnowszą wersją.
            let newest = candidates[name].first().map(|c| c.version.as_str());
            if newest != Some(ver.as_str()) {
                println!("  {} {}: using {} instead of latest ({}) to avoid a conflict",
                         "⚠".bright_black(), name, ver.white(), newest.unwrap_or("?"));
            }
        }
    }

    println!("  {} Resolved: {}", "✔".red(),
             assignment.iter().map(|(n, (v, _))| format!("{}@{}", n, v)).collect::<Vec<_>>().join(", "));
    Ok(assignment)
}

pub(crate) fn backtrack_solve(
    order: &[String], idx: usize,
    candidates: &HashMap<String, Vec<Candidate>>,
    state: &State,
    assignment: &mut HashMap<String, (String, Manifest)>,
    steps: &mut usize,
) -> bool {
    if idx == order.len() { return true; }
    let name = &order[idx];

    for cand in &candidates[name] {
        if *steps == 0 { return false; }
        *steps -= 1;

        // Konflikt z tym, co JUŻ jest zainstalowane w systemie.
        let installed_conflict = !state.check_conflicts(name, &cand.manifest.conflicts).is_empty();

        // Konflikt z tym, co solver już przypisał wcześniej w tym samym wsadzie.
        let batch_conflict = assignment.iter().any(|(other_name, (_, other_manifest))| {
            cand.manifest.conflicts.iter().any(|c| split_pkg_ver(c).0 == *other_name)
                || other_manifest.conflicts.iter().any(|c| split_pkg_ver(c).0 == *name)
        });

        if installed_conflict || batch_conflict {
            continue;
        }

        assignment.insert(name.clone(), (cand.version.clone(), cand.manifest.clone()));
        if backtrack_solve(order, idx + 1, candidates, state, assignment, steps) {
            return true;
        }
        assignment.remove(name);
    }
    false
}

fn manifest_at_commit(repo: &Repository, oid: Oid) -> Result<Manifest> {
    let commit = repo.find_commit(oid).into_diagnostic()?;
    let tree   = commit.tree().into_diagnostic()?;
    let tmp    = tempfile::tempdir().into_diagnostic()?;
    extract_manifest_only(repo, &tree, tmp.path())?;
    Manifest::load_from_path(tmp.path().to_str().unwrap())
}

fn split_spec(spec: &str) -> (String, Option<String>) {
    if spec.contains('@') {
        let mut parts = spec.splitn(2, '@');
        (parts.next().unwrap().to_string(), Some(parts.next().unwrap().to_string()))
    } else {
        (spec.to_string(), None)
    }
}

/// Wyciąga tylko `info.hk` z drzewa gita do `dest` (bez pełnego checkoutu) —
/// używane przez preflight, żeby nie kopiować całej zawartości repo tylko po
/// to, by przeczytać manifest.
fn extract_manifest_only(repo: &Repository, tree: &git2::Tree, dest: &Path) -> Result<()> {
    let entry  = tree.get_path(Path::new("info.hk")).into_diagnostic()?;
    let blob   = repo.find_blob(entry.id()).into_diagnostic()?;
    fs::write(dest.join("info.hk"), blob.content()).into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// install_single
// ---------------------------------------------------------------------------

/// Zwraca jak wrapper/`.desktop` powinien wołać hpm. Preferuje gołe `hpm`
/// (rozwiązywane przez PATH w momencie URUCHOMIENIA wrappera, nie w
/// momencie instalacji) — patrz komentarz przy miejscu wywołania. Jeśli
/// `hpm` nie jest w ogóle na PATH (typowe przy budowaniu z sourców i
/// testowaniu bez wcześniejszej instalacji), spada do `current_exe()` z
/// ostrzeżeniem, żeby wrapper przynajmniej zadziałał w tej sesji.
pub(crate) fn which_hpm_for_wrappers() -> String {
    let on_path = std::process::Command::new("which").arg("hpm").output()
        .map(|o| o.status.success()).unwrap_or(false);
    if on_path {
        return "hpm".to_string();
    }
    match std::env::current_exe() {
        Ok(p) => {
            eprintln!("  {} 'hpm' is not on PATH — wrappers will point at {} for now.",
                      "⚠".bright_black(), p.display());
            eprintln!("    Put hpm on PATH (e.g. {}) and reinstall to make wrappers portable.",
                      "~/.local/bin".dimmed());
            p.display().to_string()
        }
        Err(_) => "hpm".to_string(),
    }
}

pub fn install_single(
    pkg_name: &str, version: Option<&str>,
    repo_mgr: &RepoManager, state: &mut State, manually_installed: bool,
) -> Result<()> {
    let pkg_url = repo_mgr.get_package_url(pkg_name)
        .ok_or_else(|| miette::miette!("Package '{}' not found", pkg_name))?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner().template("{spinner:.red} {msg}").unwrap());
    pb.set_message(format!("Fetching {}...", pkg_name.white()));

    let repo_path = repo_mgr.clone_package_repo(pkg_name, pkg_url)?;
    let repo      = Repository::open(&repo_path).into_diagnostic()?;
    let tags      = repo.tag_names(None).into_diagnostic()?;
    let (selected_version, commit_oid) = resolve_version(&repo, &tags, version, pkg_name)?;

    pb.set_message(format!("Extracting {}@{}...", pkg_name.white(), selected_version.red()));
    let checkout_dir = tempfile::tempdir().into_diagnostic()?;
    let commit       = repo.find_commit(commit_oid).into_diagnostic()?;
    let tree         = commit.tree().into_diagnostic()?;
    extract_tree(&repo, &tree, checkout_dir.path())?;

    finish_install(pkg_name, &selected_version, checkout_dir.path(),
                    repo_mgr, state, manually_installed, &pb)
}

/// `hpm install <pkg> --release [<tag>]` — zamiast klonować całe repo git,
/// pobiera gotowe archiwum `.hpm` (patrz `hpm build`) dołączone do GitHub
/// Release repozytorium zarejestrowanego w repo.json/repo-list.json (URL
/// pakietu w indeksie NIE zmienia się — to musi być zwykły URL repo GitHub;
/// hpm sam dokłada `/releases/latest` albo `/releases/tags/<tag>`).
///
/// Szybsze niż pełny `git clone` dla dużych repo, i pozwala twórcy pakietu
/// dystrybuować podpisany/zweryfikowany binarny artefakt zamiast surowego
/// źródła. Weryfikuje sumę kontrolną `.sha256` jeśli jest dołączona jako
/// osobny asset obok `.hpm`.
pub fn install_single_from_release(
    pkg_name: &str, tag: Option<&str>,
    repo_mgr: &RepoManager, state: &mut State, manually_installed: bool,
    require_signed: bool,
) -> Result<()> {
    let pkg_url = repo_mgr.get_package_url(pkg_name)
        .ok_or_else(|| miette::miette!("Package '{}' not found", pkg_name))?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner().template("{spinner:.red} {msg}").unwrap());
    pb.set_message(format!("Resolving GitHub release for {}...", pkg_name.white()));

    let (owner, repo_name) = crate::repo::parse_github_owner_repo(pkg_url)
        .ok_or_else(|| miette::miette!(
            "--release requires a github.com repo URL in repo.json for '{}' (got: {})",
            pkg_name, pkg_url
        ))?;

    let release = crate::repo::fetch_github_release(&owner, &repo_name, tag)?;
    let hpm_asset = release.assets.iter()
        .find(|a| a.name.ends_with(".hpm"))
        .ok_or_else(|| miette::miette!(
            "Release '{}' of {}/{} has no .hpm asset.\n  \
  The maintainer needs to run `hpm build --output name.hpm` and attach it to the Release.",
            release.tag_name, owner, repo_name
        ))?;
    let sha_asset = release.assets.iter().find(|a| a.name == format!("{}.sha256", hpm_asset.name));
    let sig_asset = release.assets.iter().find(|a| a.name == format!("{}.sig", hpm_asset.name));

    pb.set_message(format!("Downloading {}...", hpm_asset.name.white()));
    let tmp_archive = tempfile::NamedTempFile::new().into_diagnostic()?;
    crate::utils::download_file(&hpm_asset.browser_download_url, tmp_archive.path().to_str().unwrap())?;

    if let Some(sha) = sha_asset {
        pb.set_message("Verifying checksum...");
        let tmp_sha = tempfile::NamedTempFile::new().into_diagnostic()?;
        crate::utils::download_file(&sha.browser_download_url, tmp_sha.path().to_str().unwrap())?;
        let expected = fs::read_to_string(tmp_sha.path()).into_diagnostic()?
            .split_whitespace().next().unwrap_or_default().to_string();
        let actual = crate::utils::compute_file_hash(tmp_archive.path())?;
        if !expected.is_empty() && expected != actual {
            bail!(
                "Checksum mismatch for {}!\n  expected: {}\n  actual:   {}\n  \
  Refusing to install a package whose contents don't match its published checksum.",
                hpm_asset.name, expected, actual
            );
        }
    } else {
        eprintln!("  {} No .sha256 asset alongside {} — integrity not verified.",
                  "⚠".bright_black(), hpm_asset.name);
    }

    // GPG — odłączony podpis nad CAŁYM archiwum .hpm (produkowany przez
    // `hpm build --sign <key-id>`), weryfikowany tym samym mechanizmem
    // (trusted keyring) co `hpm verify`. W przeciwieństwie do sumy sha256
    // (integralność — "plik nie został uszkodzony/podmieniony w locie"),
    // podpis GPG to autentyczność — "ten konkretny klucz naprawdę to zbudował".
    if let Some(sig) = sig_asset {
        pb.set_message("Verifying GPG signature...");
        let tmp_sig = tempfile::NamedTempFile::new().into_diagnostic()?;
        crate::utils::download_file(&sig.browser_download_url, tmp_sig.path().to_str().unwrap())?;
        match crate::commands::verify::verify_gpg_signature(tmp_archive.path(), tmp_sig.path()) {
            Ok(signer) => {
                pb.suspend(|| println!("  {} GPG signature OK, signed by: {}", "✔".red(), signer.white()));
            }
            Err(e) => {
                bail!(
                    "GPG signature verification FAILED for {}: {}\n  \
  Refusing to install a package whose signature doesn't check out.",
                    hpm_asset.name, e
                );
            }
        }
    } else if require_signed {
        bail!(
            "--require-signed was given, but {} has no .sig asset attached to this release.\n  \
  Refusing to install an unsigned package.",
            hpm_asset.name
        );
    } else {
        eprintln!("  {} No .sig asset alongside {} — not GPG-signed, authenticity not verified.",
                  "⚠".bright_black(), hpm_asset.name);
    }

    pb.set_message(format!("Extracting {}...", hpm_asset.name.white()));
    let checkout_dir = tempfile::tempdir().into_diagnostic()?;
    let status = std::process::Command::new("tar")
        .arg("-xf").arg(tmp_archive.path())
        .arg("-C").arg(checkout_dir.path())
        .status().into_diagnostic()?;
    if !status.success() {
        bail!("Failed to extract {} (needs `tar` with zstd support — `apt install zstd`)", hpm_asset.name);
    }

    let manifest = Manifest::load_from_path(checkout_dir.path().to_str().unwrap())
        .map_err(|e| miette::miette!("{} does not contain a valid info.hk: {}", hpm_asset.name, e))?;
    let selected_version = manifest.version.clone();

    finish_install(pkg_name, &selected_version, checkout_dir.path(),
                    repo_mgr, state, manually_installed, &pb)
}

/// Wspólny ogon instalacji — dzielony przez ścieżkę "git clone" (`install_single`)
/// i ścieżkę "GitHub Release .hpm" (`install_single_from_release`). Wszystko
/// od tego miejsca operuje wyłącznie na już rozpakowanym katalogu źródłowym
/// (`src_dir`) i nie wie / nie obchodzi go skąd się tam wziął.
fn finish_install(
    pkg_name: &str, selected_version: &str, src_dir: &Path,
    repo_mgr: &RepoManager, state: &mut State, manually_installed: bool,
    pb: &ProgressBar,
) -> Result<()> {
    let selected_version = selected_version.to_string();
    pb.set_message("Reading manifest...");
    let manifest = Manifest::load_from_path(src_dir.to_str().unwrap())?;

    // Arch validation
    crate::manifest::check_arch_compatibility(&manifest.arch)?;

    let build_cfg = BuildConfig::load_from_dir(src_dir);

    // Conflict check
    let violations = state.check_conflicts(pkg_name, &manifest.conflicts);
    if !violations.is_empty() {
        bail!("Cannot install '{}': conflicts:\n{}", pkg_name,
              violations.iter().map(|v| format!("  ✗ {}", v)).collect::<Vec<_>>().join("\n"));
    }

    // Deps
    if !manifest.deps.is_empty() {
        pb.set_message("Resolving dependencies...");
        for (dep_name, dep_req) in &manifest.deps {
            let already_ok = state.packages.get(dep_name)
                .map(|vs| vs.keys().any(|v| crate::utils::satisfies(v, dep_req)))
                .unwrap_or(false);
            if !already_ok {
                if state.packages.get(dep_name).map(|vs| !vs.is_empty()).unwrap_or(false) {
                    println!("\n  {} dep {} requires {} — updating",
                             "⚠".bright_black(), dep_name.white(), dep_req);
                }
                println!("\n  {} Installing dep: {}{}", "→".bright_black(), dep_name.white(),
                         if dep_req.is_empty() { String::new() } else { format!(" ({})", dep_req) });
                let dep_ver = if dep_req.is_empty() || dep_req.starts_with('>') || dep_req.starts_with('=') { None }
                              else { Some(dep_req.as_str()) };
                install_single(dep_name, dep_ver, repo_mgr, state, false)?;
            }
        }
    }

    // Build deps
    let mut build_deb = manifest.build.deb_deps.clone();
    if let Some(ref cfg) = build_cfg {
        for d in &cfg.build_deps { if !build_deb.contains(d) { build_deb.push(d.clone()); } }
    }
    if !build_deb.is_empty() {
        pb.set_message("Installing build deps...");
        crate::utils::ensure_deb_packages(&build_deb)?;
    }

    // Build
    let contents_src = if let Some(ref cfg) = build_cfg {
        run_build_config(cfg, src_dir, &selected_version, &manifest, &pb, pkg_name)?
    } else {
        run_classic_build(src_dir, &manifest, &pb)?;
        src_dir.join("contents")
    };

    if !contents_src.exists() {
        bail!("No 'contents/' for '{}@{}'", pkg_name, selected_version);
    }

    // Pre-install hook
    if manifest.has_hooks {
        let store_path_str = format!("{}{}/{}", crate::store_path(), pkg_name, selected_version);
        let ctx = crate::hooks::HookContext {
            pkg_name, pkg_version: &selected_version,
            store_path: &store_path_str, old_version: None,
        };
        crate::hooks::run_hook(src_dir, crate::hooks::HookKind::PreInstall, &ctx, &manifest)?;
    }

    // Atomic staging — zarejestruj w SIGINT registry
    let dest_dir    = Path::new(crate::store_path()).join(pkg_name).join(&selected_version);
    let staging_dir = Path::new(crate::store_path()).join(pkg_name)
        .join(format!(".staging-{}", selected_version));
    if staging_dir.exists() { let _ = fs::remove_dir_all(&staging_dir); }
    fs::create_dir_all(&staging_dir).into_diagnostic()?;

    // Zarejestruj staging do cleanup przy SIGINT
    register_staging(&staging_dir);

    let stage_result = (|| -> Result<()> {
        copy_dir_all(&contents_src, &staging_dir)?;
        make_all_binaries_executable(&staging_dir, &manifest)?;
        let manifest_src = src_dir.join("info.hk");
        if manifest_src.exists() {
            fs::copy(&manifest_src, staging_dir.join("info.hk")).into_diagnostic()?;
        }
        // Kopiuj hooki do store
        crate::hooks::install_hooks(src_dir, &staging_dir)?;
        Ok(())
    })();

    if let Err(e) = stage_result {
        let _ = fs::remove_dir_all(&staging_dir);
        unregister_staging(&staging_dir);
        return Err(e);
    }

    if dest_dir.exists() { fs::remove_dir_all(&dest_dir).into_diagnostic()?; }
    fs::rename(&staging_dir, &dest_dir).into_diagnostic()?;
    unregister_staging(&staging_dir);

    // Runtime deps
    let mut runtime_deb = manifest.runtime.deb_deps.clone();
    if let Some(ref cfg) = build_cfg {
        for d in &cfg.runtime_deps { if !runtime_deb.contains(d) { runtime_deb.push(d.clone()); } }
    }
    if !runtime_deb.is_empty() {
        pb.set_message("Installing runtime deps...");
        crate::utils::ensure_deb_packages(&runtime_deb)?;
    }

    // Wrappers — ATOMOWY ZAPIS
    pb.set_message("Creating binary wrappers...");
    // BUG NAPRAWIONY (znaleziony przy realnym teście instalacji pakietu
    // 'rust'): wrapper zapisywał na stałe DOKŁADNĄ ścieżkę bieżącego binarki
    // hpm (`current_exe()`). Jeśli hpm zostanie potem przebudowany gdzie
    // indziej / przeniesiony / to był tylko tymczasowy build — KAŻDY
    // wcześniej utworzony wrapper przestaje działać, cicho wskazując na
    // nieistniejącą ścieżkę. Dokładnie ten rodzaj "wrapper nie działa".
    //
    // Zamiast tego wrapper woła po prostu `hpm` przez PATH — jak
    // `#!/usr/bin/env node` zamiast ścieżki do konkretnej instalacji node'a.
    // Bezpieczne założenie: skoro użytkownik w ogóle uruchomił `hpm
    // install`, `hpm` musiało być na PATH w tym momencie.
    let hpm_exe: String = which_hpm_for_wrappers();

    for bin_name in &manifest.bins {
        let bin_rel = if let Some(explicit) = manifest.bin_paths.get(bin_name) {
            let p = dest_dir.join(explicit);
            if p.exists() { make_executable(&p).ok(); Some(explicit.clone()) }
            else { find_binary_in_dir(&dest_dir, bin_name) }
        } else {
            find_binary_in_dir(&dest_dir, bin_name)
        };

        let bin_rel = match bin_rel {
            Some(r) => { make_executable(&dest_dir.join(&r)).ok(); r }
            None    => { pb.suspend(|| print_binary_not_found_help(&dest_dir, bin_name)); continue; }
        };

        let wrapper_name = match resolve_wrapper_name(bin_name, pkg_name)? {
            Some(n) => n,
            None    => {
                println!("  {} Skipped '{}'. Run: {} run {} {}",
                         "ℹ".white(), bin_name, hpm_exe, pkg_name, bin_rel);
                continue;
            }
        };

        let wrapper_path = Path::new(crate::bin_dir()).join(&wrapper_name);
        let content = format!(
            "#!/bin/sh\nexec {} run {} {} \"$@\"\n",
            hpm_exe, pkg_name, bin_rel
        );
        // ATOMOWY ZAPIS: .tmp → rename
        write_wrapper_atomic(&wrapper_path, &content)?;

        if wrapper_name == *bin_name {
            println!("  {} Wrapper: {} → {}/{}/{}",
                     "✔".red(), bin_name.white(), pkg_name, selected_version, bin_rel.dimmed());
        } else {
            println!("  {} Wrapper: {} (as {}) → {}/{}/{}",
                     "✔".red(), bin_name.white(), wrapper_name.bright_black(),
                     pkg_name, selected_version, bin_rel.dimmed());
        }
    }

    // Desktop integration
    if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
        pb.set_message("Installing desktop integration...");
        install_desktop_integration_pub(&dest_dir, &manifest, pkg_name, &hpm_exe)?;
    }

    // Bash completion — NOWE
    install_completions(&dest_dir, bin_name_for_pkg(&manifest, pkg_name))?;

    // Post-install hook
    if manifest.has_hooks {
        let store_path_str = format!("{}{}/{}", crate::store_path(), pkg_name, selected_version);
        let ctx = crate::hooks::HookContext {
            pkg_name, pkg_version: &selected_version,
            store_path: &store_path_str, old_version: None,
        };
        crate::hooks::run_hook(&dest_dir, crate::hooks::HookKind::PostInstall, &ctx, &manifest)?;
    }

    let depends_on: HashSet<String> = manifest.deps.iter().map(|(name, _)| {
        state.get_current_version(name)
            .map(|ver| format!("{}@{}", name, ver))
            .unwrap_or_else(|| name.clone())
    }).collect();
    let conflicts_with: HashSet<String> = manifest.conflicts.iter().cloned().collect();
    let checksum = compute_dir_hash(&dest_dir).unwrap_or_default();
    state.update_package(pkg_name, &selected_version, &checksum,
                         manually_installed, depends_on, conflicts_with);

    let current_link = Path::new(crate::store_path()).join(pkg_name).join("current");
    let _ = fs::remove_file(&current_link);
    std::os::unix::fs::symlink(&selected_version, &current_link).into_diagnostic()?;

    // Kompresja store'u w stylu Snapa (SquashFS + squashfuse, lz4 —
    // zmierzony narzut wykonania w granicach szumu pomiarowego). Best-effort:
    // brak `mksquashfs` po prostu zostawia pakiet nieskompresowany, nigdy nie
    // blokuje instalacji.
    match crate::squash::compress_after_install(&dest_dir) {
        Ok(true) => {
            let sidecar_size = crate::squash::on_disk_size(&dest_dir);
            pb.suspend(|| println!("  {} Compressed store ({})", "✔".red(),
                                    crate::commands::clean::human_bytes(sidecar_size).dimmed()));
        }
        Ok(false) => {} // mksquashfs niedostępne — cicho, pakiet działa nieskompresowany
        Err(e) => {
            eprintln!("  {} Could not compress package store ({}) — continuing uncompressed",
                      "⚠".bright_black(), e);
        }
    }

    pb.finish_with_message(format!(
        "{} {}@{} installed successfully",
        "✔".red(), pkg_name.white(), selected_version.red()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Bash/Zsh/Fish completion — NOWE
// Jeśli pakiet ma plik completions/<name>.bash (lub .zsh, .fish) — instaluj.
// ---------------------------------------------------------------------------

fn install_completions(dest_dir: &Path, bin_name: &str) -> Result<()> {
    let completions_dir = dest_dir.join("completions");
    if !completions_dir.exists() { return Ok(()); }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root"));
    let targets: [(&str, PathBuf); 3] = [
        (".bash", home.join(".local/share/bash-completion/completions")),
        (".zsh",  home.join(".local/share/zsh/site-functions")),
        (".fish", home.join(".config/fish/completions")),
    ];

    for (ext, install_dir) in &targets {
        let src = completions_dir.join(format!("{}{}", bin_name, ext));
        if !src.exists() { continue; }
        if fs::create_dir_all(install_dir).is_ok() {
            let dst = install_dir.join(format!("{}{}", bin_name, ext));
            if fs::copy(&src, &dst).is_ok() {
                println!("  {} Completion: {} → {}", "✔".red(), src.file_name().unwrap_or_default().to_string_lossy().dimmed(), install_dir.display().to_string().dimmed());
            }
        }
    }
    Ok(())
}

fn bin_name_for_pkg<'a>(manifest: &'a Manifest, pkg_name: &'a str) -> &'a str {
    manifest.bins.first().map(|s| s.as_str()).unwrap_or(pkg_name)
}

// ---------------------------------------------------------------------------
// Desktop integration pub API
// ---------------------------------------------------------------------------

pub fn install_desktop_integration_pub(
    dest_dir: &Path, manifest: &Manifest,
    pkg_name: &str, hpm_exe: &str,
) -> Result<()> {
    install_desktop_integration(dest_dir, manifest, pkg_name, hpm_exe)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn make_all_binaries_executable(dir: &Path, manifest: &Manifest) -> Result<()> {
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
            if path.is_dir() { make_scripts_executable_recursive(&path); }
            else {
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

fn print_binary_not_found_help(dest_dir: &Path, bin_name: &str) {
    eprintln!("{} Binary '{}' not found in installed files.", "⚠".bright_black(), bin_name.white());
    let all   = list_all_files(dest_dir);
    let execs: Vec<_> = all.iter().filter(|p| {
        p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }).collect();
    if execs.is_empty() {
        eprintln!("  No executables. Fix: git update-index --chmod=+x contents/bin/<binary>");
    } else {
        eprintln!("  Executables found:");
        for f in &execs {
            if let Ok(rel) = f.strip_prefix(dest_dir) { eprintln!("    {}", rel.display()); }
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
    let desktop   = &manifest.desktop;
    let icon_name = install_icon(dest_dir, manifest, pkg_name)?;
    fs::create_dir_all(crate::desktop_dir()).into_diagnostic()?;
    let desktop_path = Path::new(crate::desktop_dir()).join(format!("{}.desktop", pkg_name));
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
    let categories = if !desktop.categories.is_empty() { desktop.categories.clone() } else { "Utility;".to_string() };
    let comment    = if !desktop.comment.is_empty()    { desktop.comment.clone()    } else { manifest.summary.clone() };
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
    let _ = std::process::Command::new("update-desktop-database").arg(crate::desktop_dir()).status();
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
            let td = Path::new(crate::icon_dir()).join("scalable/apps");
            fs::create_dir_all(&td).into_diagnostic()?;
            fs::copy(&src, td.join(format!("{}.svg", pkg_name))).into_diagnostic()?;
        } else {
            let td = Path::new(crate::icon_dir()).join("256x256/apps");
            fs::create_dir_all(&td).into_diagnostic()?;
            fs::copy(&src, td.join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
            fs::create_dir_all(crate::pixmap_dir()).into_diagnostic()?;
            fs::copy(&src, Path::new(crate::pixmap_dir()).join(format!("{}.{}", pkg_name, ext))).into_diagnostic()?;
        }
        let _ = std::process::Command::new("gtk-update-icon-cache").args(["-f", "-t", crate::icon_dir()]).status();
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
            let suffix = line.trim_start_matches("Exec=").split_whitespace().skip(1)
                .filter(|t| t.starts_with('%')).collect::<Vec<_>>().join(" ");
            if suffix.is_empty() { format!("Exec={}", new_exec) }
            else                  { format!("Exec={} {}", new_exec, suffix) }
        } else { line.to_string() }
    }).collect::<Vec<_>>().join("\n");
    fs::write(path, patched + "\n").into_diagnostic()?;
    Ok(())
}

fn find_file_by_ext(dir: &Path, ext: &str) -> Option<PathBuf> {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() { if let Some(f) = find_file_by_ext(&path, ext) { return Some(f); } }
            else if path.extension().and_then(|e| e.to_str()) == Some(ext) { return Some(path); }
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
                if blob.content().starts_with(b"#!") { make_executable(&entry_path)?; }
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
// Build
// ---------------------------------------------------------------------------

pub(crate) fn run_classic_build(src_dir: &Path, manifest: &Manifest, pb: &ProgressBar) -> Result<()> {
    let build_script = src_dir.join("build.info");
    if build_script.exists() {
        pb.set_message("Running build.info...");
        make_executable(&build_script)?;
        crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest, &["./build.info".to_string()])?;
    } else if !manifest.build.commands.is_empty() {
        pb.set_message("Building package...");
        crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest, &manifest.build.commands)?;
    }
    Ok(())
}

pub(crate) fn run_build_config(
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
            let tmp = tempfile::NamedTempFile::new().into_diagnostic()?;
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
            crate::vlog!("build.toml [source=build]: src_dir={}, output={}", src_dir.display(), output);
            crate::vlog!("build commands ({} total):\n{}", commands.len(),
                commands.iter().enumerate().map(|(i, c)| format!("  [{}] {c}", i + 1)).collect::<Vec<_>>().join("\n"));
            for (k, v) in &cfg.env {
                crate::vlog!("build env: {}={}", k, v);
                std::env::set_var(k, v);
            }
            let script = src_dir.join("_hpm_build.sh");
            fs::write(&script, format!("#!/bin/sh\nset -e\n{}", commands.join("\n"))).into_diagnostic()?;
            make_executable(&script)?;
            crate::sandbox::run_commands(src_dir.to_str().unwrap(), manifest, &["./_hpm_build.sh".to_string()])?;
            let _ = fs::remove_file(&script);
            let out = src_dir.join(output);
            crate::vlog!("build finished; expecting output at {} (exists={})", out.display(), out.exists());
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
    repo: &Repository, tags: &git2::string_array::StringArray,
    version: Option<&str>, pkg_name: &str,
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
    eprintln!("{} No tags for '{}', installing from HEAD.", "⚠".bright_black(), pkg_name);
    let head   = repo.head().into_diagnostic()?;
    let commit = head.peel_to_commit().into_diagnostic()?;
    Ok(("HEAD".to_string(), commit.id()))
}
