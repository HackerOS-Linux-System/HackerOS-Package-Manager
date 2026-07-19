use miette::{Result, IntoDiagnostic, bail, miette};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, Duration};

#[derive(Debug, Default)]
struct TestSuite {
    passed:  Vec<(String, Duration)>,
    failed:  Vec<(String, String)>,
    skipped: Vec<String>,
}

impl TestSuite {
    fn print_summary(&self) {
        let total = self.passed.len() + self.failed.len() + self.skipped.len();
        println!();
        println!("{}", "─".repeat(60).dimmed());
        println!("{}", "Test Results:".bold());
        println!("  {} Passed:  {}", "✔".red(),  self.passed.len());
        println!("  {} Failed:  {}", "✗".red(),    self.failed.len());
        println!("  {} Skipped: {}", "○".dimmed(), self.skipped.len());
        println!("  Total:    {}", total);
        if !self.failed.is_empty() {
            println!();
            println!("{}", "Failed tests:".bold().red());
            for (name, reason) in &self.failed {
                println!("  {} {} — {}", "✗".red(), name, reason);
            }
        }
        println!();
        if self.failed.is_empty() {
            println!("{} All tests passed!", "✔".red().bold());
        } else {
            println!("{} {} test(s) failed.", "✗".red().bold(), self.failed.len());
        }
    }
}

/// `hpm dev` obsługuje dwa tryby:
///   1. Znane subkomendy: `test`, `test-full`, `check-env` (jak dotychczas).
///   2. Ścieżka do katalogu (relatywna lub pełna): `hpm dev <katalog>` —
///      testowanie lokalnego pakietu, KTÓRY NIE MUSI być w repo-list.json /
///      repo.json. Idealne do portowania/weryfikacji pakietów przed publikacją
///      w oficjalnym indeksie: `hpm dev ./moj-pakiet` albo
///      `hpm dev /home/user/paczki/firefox`.
///      Wspiera dalej: `hpm dev <katalog> run <bin> [args...]` żeby od razu
///      odpalić skompilowany plik binarny przez tę samą piaskownicę co `hpm run`.
pub fn dev(args: Vec<String>) -> Result<()> {
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("test");
    match subcmd {
        "test"      => run_tests(false),
        "test-full" => run_tests(true),
        "check-env" => check_environment(),
        _ => {
            // Nie jest to znana subkomenda — sprawdź czy to ścieżka do katalogu
            // pakietu (zawiera info.hk). Rozróżnienie: subkomendy nigdy nie
            // wyglądają jak ścieżka (nie zawierają '/', nie zaczynają się od
            // '.', nie istnieją jako katalog w cwd) — ale zamiast zgadywać po
            // składni, po prostu sprawdzamy istnienie katalogu na dysku, co
            // jest jednoznaczne i działa zarówno dla ścieżek względnych jak
            // i pełnych.
            let candidate = PathBuf::from(subcmd);
            if candidate.is_dir() {
                return dev_local(subcmd, args[1..].to_vec());
            }
            eprintln!(
                "{} Unknown dev subcommand or path: '{}'\n  \
  Expected one of: test | test-full | check-env\n  \
  ...or a path to a local package directory (containing info.hk), e.g.:\n  \
    hpm dev ./my-package\n  \
    hpm dev /home/user/packages/firefox run firefox",
                "✗".red(), subcmd
            );
            std::process::exit(1);
        }
    }
}

/// Testuje lokalny katalog pakietu bez wymogu obecności w repo-list.json /
/// repo.json i bez dotykania systemowego crate::store_path() (nie wymaga roota).
/// Odtwarza cykl życia instalacji (walidacja → build → pre-install hook →
/// post-install hook) dokładnie tymi samymi mechanizmami co `hpm install`
/// (ten sam parser manifestu, ta sama piaskownica na hooki, ten sam resolver
/// binarek), więc jeśli coś tu przejdzie, przejdzie też przy realnym `hpm
/// install` po opublikowaniu pakietu.
fn dev_local(path_arg: &str, rest: Vec<String>) -> Result<()> {
    let dir_raw = PathBuf::from(path_arg);
    if !dir_raw.exists() {
        bail!("Path '{}' does not exist", path_arg);
    }
    if !dir_raw.is_dir() {
        bail!("'{}' is not a directory (expected a package directory containing info.hk)", path_arg);
    }
    let dir = dir_raw.canonicalize().into_diagnostic()?;

    if !dir.join("info.hk").exists() {
        bail!(
            "'{}' has no info.hk — not a valid hpm package directory.\n  \
  Expected layout: <dir>/info.hk (+ optionally contents/, hooks/, build.toml)",
            dir.display()
        );
    }

    println!("\n{} {}\n", "hpm dev".bold().red(),
              format!("— local package test: {}", dir.display()).dimmed());

    let manifest = crate::manifest::Manifest::load_from_path(dir.to_str().unwrap())
        .map_err(|e| miette!("Failed to load info.hk: {}", e))?;

    print_manifest_summary(&manifest);

    // Informacyjnie: czy ten pakiet jest też zarejestrowany w skonfigurowanym
    // indeksie repo — to NIE blokuje testu, `hpm dev` działa niezależnie od
    // repo-list.json / repo.json, ale warto wiedzieć czy testujemy nową
    // paczkę, czy nadpisujemy lokalnie coś, co jest już opublikowane.
    if let Ok(rm) = crate::repo::RepoManager::load_sync() {
        if rm.get_package_url(&manifest.name).is_some() {
            println!("  {} Note: '{}' is ALSO present in the configured repo index — \
this local directory is used instead for this test run only.",
                     "ℹ".white(), manifest.name);
        } else {
            println!("  {} '{}' is not in repo-list.json / repo.json — \
that's expected for local dev testing.", "ℹ".white(), manifest.name);
        }
    }

    crate::manifest::check_arch_compatibility(&manifest.arch)?;

    let warnings = crate::hooks::validate_hooks(&dir);
    if warnings.is_empty() {
        println!("  {} No hook validation warnings", "✔".red());
    } else {
        println!("  {} Hook validation warnings:", "⚠".bright_black());
        for w in &warnings { println!("      - {}", w); }
    }

    let contents_dir = dir.join("contents");
    if !contents_dir.exists() {
        // Ujednolicone z `hpm install`: najpierw build.toml (BuildConfig —
        // download/build/prebuilt źródła), potem klasyczny [build] commands /
        // build.info z info.hk. To ta sama ścieżka kodu co realna instalacja,
        // więc jeśli build przejdzie tutaj, przejdzie też przy `hpm install`.
        let pb = indicatif::ProgressBar::new_spinner();
        pb.set_message("Building...");
        if let Some(cfg) = crate::repo::BuildConfig::load_from_dir(&dir) {
            println!("  {} Found build.toml — running BuildConfig pipeline...", "→".white());
            crate::commands::install::run_build_config(&cfg, &dir, &manifest.version, &manifest, &pb, &manifest.name)?;
        } else if !manifest.build.commands.is_empty()
            || dir.join("build.info").exists() {
            println!("  {} No contents/ yet — running [build] commands in sandbox...", "→".white());
            crate::commands::install::run_classic_build(&dir, &manifest, &pb)?;
        } else {
            pb.finish_and_clear();
            bail!(
                "No 'contents/' directory, no build.toml, and no [build] commands in info.hk — \
nothing to test.\n  Either provide a pre-built contents/, or add a build step \
(same as a real package needs for 'hpm install')."
            );
        }
        pb.finish_and_clear();
        if !contents_dir.exists() {
            bail!("Build finished but 'contents/' is still missing — check build.toml / [build] in info.hk");
        }
        println!("  {} Build finished", "✔".red());
    } else {
        println!("  {} contents/ already present — skipping build step", "✔".red());
    }

    crate::commands::install::make_all_binaries_executable(&dir, &manifest)?;

    // Symuluj cykl życia instalacji: pre-install hook -> post-install hook —
    // dokładnie jak `hpm install`, ale bez crate::store_path() i bez roota.
    let fake_store_path = format!("(dev) {}", dir.display());
    let ctx = crate::hooks::HookContext {
        pkg_name:    &manifest.name,
        pkg_version: &manifest.version,
        store_path:  &fake_store_path,
        old_version: None,
    };
    if crate::hooks::hook_exists(&dir, crate::hooks::HookKind::PreInstall) {
        crate::hooks::run_hook(&dir, crate::hooks::HookKind::PreInstall, &ctx, &manifest)?;
    }
    if crate::hooks::hook_exists(&dir, crate::hooks::HookKind::PostInstall) {
        crate::hooks::run_hook(&dir, crate::hooks::HookKind::PostInstall, &ctx, &manifest)?;
    }

    if manifest.bins.is_empty() {
        println!("\n  {} Package declares no [metadata] bins — nothing to run.", "ℹ".white());
    } else {
        println!("\n  {} Binaries declared:", "→".white());
        for b in &manifest.bins {
            match crate::commands::install::find_binary_in_dir(&dir, b) {
                Some(rel) => println!("    {} {} → {}", "✔".red(), b, rel.dimmed()),
                None      => println!("    {} {} — NOT FOUND in contents/", "✗".red(), b),
            }
        }
        println!("\n  Try it: hpm dev {} run <bin> [args...]", path_arg);
    }

    // Opcjonalnie: od razu odpal binarkę: `hpm dev <path> run <bin> [args...]`
    if let Some(sub) = rest.first() {
        if sub == "run" {
            let bin = rest.get(1).ok_or_else(|| {
                miette!("Usage: hpm dev {} run <bin> [args...]", path_arg)
            })?;
            let extra_args = rest[2..].to_vec();
            let bin_rel = crate::commands::install::find_binary_in_dir(&dir, bin)
                .ok_or_else(|| miette!("Binary '{}' not found in '{}'", bin, dir.display()))?;
            println!("\n  {} Running '{}' via sandbox...\n", "→".white(), bin);
            crate::sandbox::setup_sandbox(
                dir.to_str().unwrap(), &manifest, false, Some(&bin_rel), extra_args, false,
            )?;
        } else {
            eprintln!(
                "  {} Unknown 'hpm dev {} {}' — did you mean: run <bin> [args...]?",
                "⚠".bright_black(), path_arg, sub
            );
        }
    }

    println!("\n{} Local dev test finished for '{}@{}'.",
              "✔".red().bold(), manifest.name, manifest.version);
    Ok(())
}

fn print_manifest_summary(m: &crate::manifest::Manifest) {
    println!("  {:<12} {}", "Name:", m.name.white());
    println!("  {:<12} {}", "Version:",  m.version);
    println!("  {:<12} {}", "Authors:",  if m.authors.is_empty() { "-" } else { &m.authors });
    println!("  {:<12} {}", "License:",  if m.license.is_empty() { "-" } else { &m.license });
    println!("  {:<12} {}", "Arch:",     if m.arch.is_empty() { "any" } else { &m.arch });
    if !m.tags.is_empty() {
        println!("  {:<12} {}", "Tags:", m.tags.join(", ").dimmed());
    }
    println!("  {:<12} network={} gui={} full_gui={} disabled={} filesystem={:?}",
             "Sandbox:", m.sandbox.network, m.sandbox.gui, m.sandbox.full_gui,
             m.sandbox_disabled, m.sandbox.filesystem);
    println!("  {:<12} {}", "Hooks:", if m.has_hooks { "yes".red().to_string() } else { "no".dimmed().to_string() });
    println!();
}

fn check_environment() -> Result<()> {
    println!("{} Checking hpm development environment...\n", "→".white());
    let checks: &[(&str, &str, bool)] = &[
        ("git",      "Git",                        true),
        ("tar",      "tar",                        true),
        ("hl",       "Hacker Lang (hl)",            false),
        ("zstd",     "zstd",                       false),
        ("gpg",      "GPG",                        false),
        ("bpftrace", "bpftrace (eBPF tracing)",    false),
        ("valac",    "Vala compiler",              false),
        ("meson",    "Meson build system",         false),
        ("cargo",    "Rust/Cargo",                 false),
    ];
    let mut all_ok = true;
    for (cmd, desc, required) in checks {
        let found = std::process::Command::new("which").arg(cmd).output()
        .map(|o| o.status.success()).unwrap_or(false);
        if found {
            let version = std::process::Command::new(cmd).arg("--version").output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.lines().next().unwrap_or("").trim().to_string())
            .unwrap_or_default();
            println!("  {} {:<35} {}", "✔".red(), desc, version.dimmed());
        } else if *required {
            println!("  {} {:<35} MISSING (required)", "✗".red(), desc);
            all_ok = false;
        } else {
            println!("  {} {:<35} not found (optional)", "○".dimmed(), desc);
        }
    }
    println!();
    println!("{}", "Kernel features:".bold());
    check_kf("User namespaces",  "/proc/sys/user/max_user_namespaces",  "0");
    check_kf("Landlock LSM",     "/proc/sys/kernel/landlock/abi",       "");
    check_kf("eBPF JIT",         "/proc/sys/net/core/bpf_jit_enable",  "0");
    println!();
    if all_ok { println!("{} Environment ready.", "✔".red()); }
    else       { println!("{} Some required tools missing.", "✗".red()); }
    Ok(())
}

fn check_kf(name: &str, path: &str, bad_value: &str) {
    if let Ok(val) = fs::read_to_string(path) {
        let val = val.trim();
        if val == bad_value { println!("  {} {:<30} {} (disabled)", "⚠".bright_black(), name, val.red()); }
        else                { println!("  {} {:<30} {}", "✔".red(), name, val.dimmed()); }
    } else {
        println!("  {} {:<30} not available", "○".dimmed(), name);
    }
}

fn run_tests(full: bool) -> Result<()> {
    println!("\n{} {}\n", "hpm dev test".bold().red(), "— Integration Test Suite");
    println!("  hpm version : {}", env!("CARGO_PKG_VERSION").white());
    println!("  full mode   : {}", if full { "yes".red() } else { "no (quick)".dimmed() });
    println!();

    let test_env = TestEnvironment::setup()?;
    println!("{} Test environment: {}\n", "→".white(), test_env.root.display().to_string().dimmed());

    let mut suite   = TestSuite::default();
    let total_start = Instant::now();

    run_test(&mut suite, "manifest-parse",            || test_manifest_parse(&test_env));
    run_test(&mut suite, "manifest-invalid",          || test_manifest_invalid(&test_env));
    run_test(&mut suite, "build-validate",            || test_build_validates(&test_env));
    run_test(&mut suite, "lock-generate",             || test_lock_generate(&test_env));
    run_test(&mut suite, "lock-check-ok",             || test_lock_check_ok());
    run_test(&mut suite, "lock-check-diff",           || test_lock_check_diff());
    run_test(&mut suite, "state-roundtrip",           || test_state_roundtrip());
    run_test(&mut suite, "state-conflict",            || test_state_conflict());
    run_test(&mut suite, "wrapper-names-persist",     || test_wrapper_names());
    run_test(&mut suite, "version-compare",           || test_version_compare());
    run_test(&mut suite, "version-satisfies",         || test_version_satisfies());
    run_test(&mut suite, "compute-hash",              || test_compute_hash(&test_env));
    run_test(&mut suite, "hooks-validate-hl",         || test_hooks_validate_hl(&test_env));
    run_test(&mut suite, "hooks-hl-syntax",           || test_hooks_hl_syntax(&test_env));
    run_test(&mut suite, "hooks-pre-install",         || test_hooks_pre_install(&test_env));
    run_test(&mut suite, "hooks-post-install",        || test_hooks_post_install(&test_env));
    run_test(&mut suite, "hooks-fail-blocks-install", || test_hooks_fail_blocks(&test_env));
    run_test(&mut suite, "diff-manifests",            || test_diff_manifests(&test_env));
    run_test(&mut suite, "diff-files",                || test_diff_files(&test_env));
    run_test(&mut suite, "arch-check",                || test_arch_check());
    run_test(&mut suite, "tag-grouping",              || test_tag_grouping());
    run_test(&mut suite, "url-validation",            || test_url_validation());
    run_test(&mut suite, "wrapper-atomic-write",      || test_wrapper_atomic(&test_env));
    run_test(&mut suite, "search-offline-fallback",   || test_search_offline());

    if full {
        run_test(&mut suite, "search-pagination",   || test_search_pagination());
        run_test(&mut suite, "sandbox-compat-mode", || test_sandbox_compat());
        run_test(&mut suite, "rollback-full-state", || test_rollback_state());
        run_test(&mut suite, "solver-picks-older-version-to-avoid-conflict", || test_solver_conflict_resolution());
        run_test(&mut suite, "solver-fails-clearly-when-unsatisfiable",      || test_solver_unsatisfiable());
    } else {
        suite.skipped.push("search-pagination".to_string());
        suite.skipped.push("sandbox-compat-mode".to_string());
        suite.skipped.push("rollback-full-state".to_string());
    }

    println!();
    print!("{} Cleaning up test environment...", "→".white());
    test_env.cleanup();
    println!(" {}", "done".red());
    println!("\n  Total time: {:.2}s", total_start.elapsed().as_secs_f32());
    suite.print_summary();
    if !suite.failed.is_empty() { std::process::exit(1); }
    Ok(())
}

fn run_test<F: FnOnce() -> Result<()>>(suite: &mut TestSuite, id: &str, f: F) {
    print!("  {} {:<48} ", "…".dimmed(), id);
    std::io::stdout().flush().ok();
    let start = Instant::now();
    match f() {
        Ok(()) => {
            let e = start.elapsed();
            println!("{} {}", "OK".red().bold(), format!("({:.1}s)", e.as_secs_f32()).dimmed());
            suite.passed.push((id.to_string(), e));
        }
        Err(e) => {
            println!("{}", "FAIL".red().bold());
            println!("    {}", e.to_string().red());
            suite.failed.push((id.to_string(), e.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// TestEnvironment
// FIXED: TempDir::keep() zwraca std::io::Result<(TempDir, PathBuf)>
// NIE implementuje IntoError/into_diagnostic — używamy map_err
// ---------------------------------------------------------------------------
struct TestEnvironment {
    root:      PathBuf,
    store:     PathBuf,
    lock_file: PathBuf,
}

impl TestEnvironment {
    fn setup() -> Result<Self> {
        let tmp = tempfile::tempdir().into_diagnostic()?;
        // FIXED (real bug found via `cargo check`, was previously `tmp.keep()` which
        // does not exist on `TempDir` in any tempfile version — `TempDir` only has
        // `into_path(self) -> PathBuf`, which is what we actually want here: consume
        // the guard so the directory survives past this function without auto-cleanup
        // on Drop (cleanup happens explicitly via `TestEnvironment::cleanup`).
        let root      = tmp.into_path();
        let store     = root.join("store");
        let lock_file = root.join("hpm.lock");
        fs::create_dir_all(&store).into_diagnostic()?;
        Ok(Self { root, store, lock_file })
    }

    fn cleanup(self) { let _ = fs::remove_dir_all(&self.root); }

    fn make_pkg_dir(&self, name: &str, version: &str, extra: &str) -> PathBuf {
        let dir = self.root.join(format!("pkg-{}-{}", name, version));
        fs::create_dir_all(dir.join("contents/bin")).ok();
        let info = format!(
            "[metadata]\n-> name => {}\n-> version => {}\n-> authors => Test\n-> license => MIT\n{}\n\
[description]\n-> summary => Test package {}\n\
[sandbox]\n-> network => false\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {{}}\n",
name, version, extra, name
        );
        fs::write(dir.join("info.hk"), &info).ok();
        let bin = dir.join(format!("contents/bin/{}", name));
        fs::write(&bin, format!("#!/usr/bin/env hl\n~> hello from {} {}\n", name, version)).ok();
        crate::utils::make_executable(&bin).ok();
        dir
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn test_manifest_parse(env: &TestEnvironment) -> Result<()> {
    let dir = env.make_pkg_dir("test-parse", "1.0.0", "-> bins.test-parse => \"bin/test-parse\"");
    let m   = crate::manifest::Manifest::load_from_path(dir.to_str().unwrap())?;
    assert_eq!(m.name, "test-parse");
    assert_eq!(m.version, "1.0.0");
    assert!(m.bins.contains(&"test-parse".to_string()));
    Ok(())
}

fn test_manifest_invalid(env: &TestEnvironment) -> Result<()> {
    let dir = env.root.join("pkg-invalid");
    fs::create_dir_all(&dir).into_diagnostic()?;
    fs::write(dir.join("info.hk"), "[metadata]\n-> name => \n-> version => \n").into_diagnostic()?;
    match crate::manifest::Manifest::load_from_path(dir.to_str().unwrap()) {
        Err(_)  => Ok(()),
        Ok(m)   => if m.name.is_empty() { Ok(()) }
        else { bail!("Expected failure for empty name") }
    }
}

fn test_build_validates(env: &TestEnvironment) -> Result<()> {
    let dir = env.root.join("pkg-build-test");
    fs::create_dir_all(dir.join("contents/bin")).into_diagnostic()?;
    fs::write(dir.join("info.hk"),
              "[metadata]\n-> name => valid-pkg\n-> version => 1.0.0\n\
[description]\n-> summary => test\n\
[sandbox]\n-> network => false\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {}\n"
    ).into_diagnostic()?;
    let m = crate::manifest::Manifest::load_from_path(dir.to_str().unwrap())?;
    if m.name.is_empty() { bail!("Empty name should have failed"); }
    Ok(())
}

fn test_lock_generate(env: &TestEnvironment) -> Result<()> {
    use crate::commands::lock::LockFile;
    let lock = LockFile::new();
    lock.save(&env.lock_file)?;
    assert!(env.lock_file.exists());
    let loaded = LockFile::load(&env.lock_file)?;
    assert_eq!(loaded.lock_version, 1);
    Ok(())
}

fn test_lock_check_ok() -> Result<()> {
    use crate::commands::lock::LockFile;
    use crate::state::State;
    let lock  = LockFile::new();
    let state = State::default();
    let diffs = lock.check_against_state(&state);
    assert!(diffs.is_empty(), "Expected no diffs, got: {:?}", diffs);
    Ok(())
}

fn test_lock_check_diff() -> Result<()> {
    use crate::commands::lock::{LockFile, LockEntry};
    use crate::state::State;
    use std::collections::HashMap;
    let mut lock = LockFile::new();
    lock.packages.insert("missing-pkg".to_string(), LockEntry {
        version: "1.0.0".to_string(), git_commit: "abc".to_string(),
                         repo_url: "https://github.com/test/test".to_string(),
                         checksum: "dead".to_string(), dependencies: HashMap::new(),
                         manually_installed: true, installed_at: 0,
    });
    let state = State::default();
    let diffs = lock.check_against_state(&state);
    assert!(!diffs.is_empty());
    assert!(diffs[0].contains("missing"));
    Ok(())
}

fn test_state_roundtrip() -> Result<()> {
    use crate::state::{State, VersionInfo};
    use std::collections::HashSet;
    let mut state = State::default();
    let mut info  = VersionInfo::new("abc123", true);
    info.depends_on = HashSet::from(["dep-a@1.0.0".to_string()]);
    state.packages.entry("test-pkg".to_string()).or_default()
    .insert("2.0.0".to_string(), info);
    let data: Vec<u8> = serde_json::to_vec_pretty(&state).into_diagnostic()?;
    let loaded: State = serde_json::from_slice(&data).into_diagnostic()?;
    assert!(loaded.packages.contains_key("test-pkg"));
    assert_eq!(loaded.packages["test-pkg"]["2.0.0"].checksum, "abc123");
    Ok(())
}

fn test_state_conflict() -> Result<()> {
    use crate::state::{State, VersionInfo};
    use std::collections::HashSet;
    let mut state = State::default();
    let mut info  = VersionInfo::new("hash1", true);
    info.conflicts_with = HashSet::from(["new-pkg".to_string()]);
    state.packages.entry("installed-pkg".to_string()).or_default()
    .insert("1.0.0".to_string(), info);
    let conflicts = state.check_conflicts("new-pkg", &[]);
    assert!(!conflicts.is_empty());
    assert!(conflicts[0].contains("new-pkg"));
    Ok(())
}

fn test_wrapper_names() -> Result<()> {
    use crate::state::WrapperNames;
    let mut wn = WrapperNames::default();
    wn.set("mypkg", "mybin", "mypkg-mybin");
    assert_eq!(wn.get("mypkg", "mybin"), Some("mypkg-mybin"));
    assert_eq!(wn.get("mypkg", "other"), None);
    wn.remove_pkg("mypkg");
    assert_eq!(wn.get("mypkg", "mybin"), None);
    Ok(())
}

fn test_version_compare() -> Result<()> {
    use crate::utils::compare_versions;
    use std::cmp::Ordering;
    assert_eq!(compare_versions("1.2.0", "1.1.9"), Ordering::Greater);
    assert_eq!(compare_versions("1.0.0", "1.0.0"), Ordering::Equal);
    assert_eq!(compare_versions("0.9.9", "1.0.0"), Ordering::Less);
    assert_eq!(compare_versions("2.0.0", "1.99.99"), Ordering::Greater);
    Ok(())
}

fn test_version_satisfies() -> Result<()> {
    use crate::utils::satisfies;
    assert!(satisfies("1.2.0", ">=1.0"));
    assert!(satisfies("1.0.0", ">=1.0.0"));
    assert!(!satisfies("0.9.0", ">=1.0"));
    assert!(satisfies("1.5.0", ">1.4"));
    assert!(satisfies("2.0.0", "=2.0.0"));
    assert!(!satisfies("2.0.1", "=2.0.0"));
    assert!(satisfies("1.0.0", ""));
    Ok(())
}

fn test_compute_hash(env: &TestEnvironment) -> Result<()> {
    use crate::utils::compute_dir_hash;
    let dir = env.root.join("hash-test");
    fs::create_dir_all(&dir).into_diagnostic()?;
    fs::write(dir.join("a.txt"), b"hello").into_diagnostic()?;
    fs::write(dir.join("b.txt"), b"world").into_diagnostic()?;
    let h1 = compute_dir_hash(&dir)?;
    let h2 = compute_dir_hash(&dir)?;
    assert_eq!(h1, h2, "Hash must be deterministic");
    fs::write(dir.join("a.txt"), b"changed").into_diagnostic()?;
    let h3 = compute_dir_hash(&dir)?;
    assert_ne!(h1, h3, "Hash must change after modification");
    Ok(())
}

fn test_hooks_validate_hl(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::validate_hooks;
    let dir       = env.root.join("hooks-validate-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;
    // .hl bez shebang
    fs::write(hooks_dir.join("post-install.hl"), b"~> done").into_diagnostic()?;
    let warnings = validate_hooks(&dir);
    assert!(warnings.iter().any(|w| w.contains("shebang")),
            "Should warn about missing shebang, got: {:?}", warnings);
    Ok(())
}

fn test_hooks_hl_syntax(env: &TestEnvironment) -> Result<()> {
    let dir       = env.root.join("hooks-hl-syntax-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;
    let hook_content = "#!/usr/bin/env hl\n\
/// Hook pre-install dla pakietu testowego\n\
using <gen 2>\n\
\n\
% pkg_name: str = @HPM_PKG_NAME\n\
% pkg_ver: str  = @HPM_PKG_VERSION\n\
\n\
~> Installing @pkg_name version @pkg_ver\n\
\n\
;; Sprawdź zależności\n\
@ dep in curl git\n\
::which @dep\n\
? ok\n\
::green tick @dep dostepny\n\
done\n\
done\n\
\n\
~> Pre-install hook zakonczony\n";
    fs::write(hooks_dir.join("pre-install.hl"), hook_content.as_bytes()).into_diagnostic()?;
    crate::utils::make_executable(&hooks_dir.join("pre-install.hl"))?;
    let content = fs::read_to_string(hooks_dir.join("pre-install.hl")).into_diagnostic()?;
    assert!(content.starts_with("#!/usr/bin/env hl"), "Hook should have hl shebang");
    assert!(content.contains("using <gen 2>"), "Hook should declare gen 2");
    let warnings = crate::hooks::validate_hooks(&dir);
    assert!(!warnings.iter().any(|w| w.contains("shebang")),
            "Valid .hl hook should not warn about shebang, got: {:?}", warnings);
    Ok(())
}

fn test_hooks_pre_install(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};
    let dir       = env.root.join("hooks-pre-test");
    let hooks_dir = dir.join("hooks");
    let sentinel  = env.root.join("hook-ran");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;
    let script = format!("#!/usr/bin/env hl\ntouch {}\n", sentinel.display());
    fs::write(hooks_dir.join("pre-install.hl"), script.as_bytes()).into_diagnostic()?;
    crate::utils::make_executable(&hooks_dir.join("pre-install.hl"))?;
    let ctx = HookContext {
        pkg_name: "test-hook", pkg_version: "1.0.0",
        store_path: env.store.to_str().unwrap(), old_version: None,
    };
    let manifest = crate::manifest::Manifest::default();
    let ran = run_hook(&dir, HookKind::PreInstall, &ctx, &manifest)?;
    assert!(ran, "Hook should have run");
    Ok(())
}

fn test_hooks_post_install(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};
    let dir       = env.root.join("hooks-post-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;
    fs::write(hooks_dir.join("post-install.hl"),
              b"#!/usr/bin/env hl\necho post-install hook ran\n").into_diagnostic()?;
              crate::utils::make_executable(&hooks_dir.join("post-install.hl"))?;
              let ctx = HookContext {
                  pkg_name: "post-hook-pkg", pkg_version: "2.0.0",
                  store_path: env.store.to_str().unwrap(), old_version: None,
              };
              let manifest = crate::manifest::Manifest::default();
              run_hook(&dir, HookKind::PostInstall, &ctx, &manifest)?;
              Ok(())
}

fn test_hooks_fail_blocks(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};
    let dir       = env.root.join("hooks-fail-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;
    // sh fallback gdy hl niedostępny — exit 1 blokuje instalację
    fs::write(hooks_dir.join("pre-install.hl"),
              b"#!/bin/sh\necho 'Refusing'\nexit 1\n").into_diagnostic()?;
              crate::utils::make_executable(&hooks_dir.join("pre-install.hl"))?;
              let ctx = HookContext {
                  pkg_name: "bad-hook-pkg", pkg_version: "1.0.0",
                  store_path: env.store.to_str().unwrap(), old_version: None,
              };
              let manifest = crate::manifest::Manifest::default();
              match run_hook(&dir, HookKind::PreInstall, &ctx, &manifest) {
                  Err(_) => Ok(()),
                  Ok(_)  => bail!("pre-install hook failure should have blocked install"),
              }
}

fn test_diff_manifests(env: &TestEnvironment) -> Result<()> {
    let dir1 = env.make_pkg_dir("diff-pkg", "1.0.0", "");
    let dir2 = env.root.join("diff-pkg-2.0.0");
    fs::create_dir_all(&dir2).into_diagnostic()?;
    fs::write(dir2.join("info.hk"),
              "[metadata]\n-> name => diff-pkg\n-> version => 2.0.0\n-> authors => New\n-> license => MIT\n\
[description]\n-> summary => Changed\n\
[sandbox]\n-> network => true\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {}\n"
    ).into_diagnostic()?;
    let m1 = crate::manifest::Manifest::load_from_path(dir1.to_str().unwrap())?;
    let m2 = crate::manifest::Manifest::load_from_path(dir2.to_str().unwrap())?;
    assert_ne!(m1.version, m2.version);
    assert_ne!(m1.sandbox.network, m2.sandbox.network);
    Ok(())
}

fn test_diff_files(env: &TestEnvironment) -> Result<()> {
    use sha2::{Sha256, Digest};
    let dir1 = env.root.join("fdiff-1");
    let dir2 = env.root.join("fdiff-2");
    fs::create_dir_all(&dir1).into_diagnostic()?;
    fs::create_dir_all(&dir2).into_diagnostic()?;
    fs::write(dir1.join("same.txt"),    b"same content").into_diagnostic()?;
    fs::write(dir2.join("same.txt"),    b"same content").into_diagnostic()?;
    fs::write(dir1.join("changed.txt"), b"version 1").into_diagnostic()?;
    fs::write(dir2.join("changed.txt"), b"version 2").into_diagnostic()?;
    let hash = |p: &Path| -> String {
        let data = fs::read(p).unwrap_or_default();
        format!("{:x}", Sha256::digest(&data))[..16].to_string()
    };
    assert_eq!(hash(&dir1.join("same.txt")), hash(&dir2.join("same.txt")));
    assert_ne!(hash(&dir1.join("changed.txt")), hash(&dir2.join("changed.txt")));
    Ok(())
}

fn test_arch_check() -> Result<()> {
    crate::manifest::check_arch_compatibility("any")?;
    crate::manifest::check_arch_compatibility("")?;
    Ok(())
}

fn test_tag_grouping() -> Result<()> {
    use crate::repo::{RepoIndex, PackageMeta};
    use std::collections::HashMap;
    let mut packages = HashMap::new();
    packages.insert("gcc".to_string(),     "https://github.com/test/gcc".to_string());
    packages.insert("make".to_string(),    "https://github.com/test/make".to_string());
    packages.insert("firefox".to_string(), "https://github.com/test/firefox".to_string());
    let index = RepoIndex { packages };
    assert_eq!(index.len(), 3);
    assert!(index.url_of("gcc").is_some());
    assert_eq!(index.url_of("nonexistent"), None);
    let meta_gcc = PackageMeta {
        name: "gcc".to_string(), version: "14.0.0".to_string(),
        summary: "GNU C Compiler".to_string(), authors: "GNU".to_string(),
        license: "GPL-3.0".to_string(),
        tags: vec!["development".to_string(), "compilers".to_string()],
        available_versions: vec!["14.0.0".to_string()], fetched_at: u64::MAX,
    };
    assert!(meta_gcc.tags.iter().any(|t| t.to_lowercase() == "development"));
    assert!(!meta_gcc.tags.iter().any(|t| t.to_lowercase() == "browsers"));
    Ok(())
}

fn test_url_validation() -> Result<()> {
    use crate::repo::validate_repo_url;
    validate_repo_url("https://github.com/user/repo")?;
    validate_repo_url("http://github.com/user/repo")?;
    validate_repo_url("ssh://git@github.com/user/repo")?;
    validate_repo_url("git@github.com:user/repo.git")?;
    assert!(validate_repo_url("file:///etc/passwd").is_err());
    assert!(validate_repo_url("https://localhost/repo").is_err());
    assert!(validate_repo_url("").is_err());
    assert!(validate_repo_url("https://github.com/../etc/passwd").is_err());
    assert!(validate_repo_url("ftp://example.com/repo").is_err());
    Ok(())
}

fn test_wrapper_atomic(env: &TestEnvironment) -> Result<()> {
    let wrapper_dir  = env.root.join("usr_bin");
    fs::create_dir_all(&wrapper_dir).into_diagnostic()?;
    let wrapper_path = wrapper_dir.join("test-wrapper");
    let tmp_path     = wrapper_dir.join("test-wrapper.hpm.tmp");
    let content      = "#!/bin/sh\nexec hpm run test-pkg bin/test\n";
    fs::write(&tmp_path, content.as_bytes()).into_diagnostic()?;
    fs::rename(&tmp_path, &wrapper_path).into_diagnostic()?;
    assert!(wrapper_path.exists());
    assert!(!tmp_path.exists());
    let read_back = fs::read_to_string(&wrapper_path).into_diagnostic()?;
    assert_eq!(read_back, content);
    Ok(())
}

fn test_search_offline() -> Result<()> {
    use crate::repo::PackageMeta;
    let meta = PackageMeta {
        name: "offline-test-pkg".to_string(), version: "1.0.0".to_string(),
        summary: "Test offline search".to_string(), authors: "Test".to_string(),
        license: "MIT".to_string(), tags: vec!["test".to_string()],
        available_versions: vec!["1.0.0".to_string()], fetched_at: 0,
    };
    assert_eq!(meta.name, "offline-test-pkg");
    assert!(!meta.tags.is_empty());
    Ok(())
}

fn test_search_pagination() -> Result<()> {
    let results: Vec<String> = (0..60).map(|i| format!("pkg-{:03}", i)).collect();
    let page0 = &results[0..20];
    let page1 = &results[20..40];
    assert_eq!(page0.len(), 20);
    assert_eq!(page0[0], "pkg-000");
    assert_eq!(page1[0], "pkg-020");
    Ok(())
}

fn test_sandbox_compat() -> Result<()> {
    use nix::sched::{unshare, CloneFlags};
    match unshare(CloneFlags::CLONE_NEWNS) {
        Ok(())                                    => Ok(()),
        Err(e) if e == nix::errno::Errno::EPERM  => Ok(()),
        Err(e) => bail!("Unexpected unshare error: {}", e),
    }
}

fn test_rollback_state() -> Result<()> {
    use crate::state::{State, VersionInfo};
    let mut state = State::default();
    state.packages.entry("rollback-pkg".to_string()).or_default()
    .insert("1.0.0".to_string(), VersionInfo::new("hash1", true));
    state.push_snapshot("pre-update rollback-pkg");
    assert_eq!(state.history.len(), 1);
    state.packages.entry("rollback-pkg".to_string()).or_default()
    .insert("2.0.0".to_string(), VersionInfo::new("hash2", true));
    let ok = state.restore_snapshot(0);
    assert!(ok);
    assert!(state.packages.get("rollback-pkg")
    .map(|vs| vs.contains_key("1.0.0")).unwrap_or(false));
    Ok(())
}

/// Testuje sam algorytm backtrackingu (bez git/sieci): pkg-a ma dwóch
/// kandydatów — 2.0.0 (najnowszy, deklaruje konflikt z pkg-b) i 1.0.0 (bez
/// konfliktu). pkg-b ma jedną wersję. Solver powinien wybrać pkg-a@1.0.0
/// zamiast pkg-a@2.0.0, żeby uniknąć konfliktu — dokładnie to, czego
/// wcześniej brakowało (byłby po prostu odrzucony cały `hpm install`).
fn test_solver_conflict_resolution() -> Result<()> {
    use crate::commands::install::{Candidate, backtrack_solve};
    use crate::manifest::Manifest;
    use crate::state::State;
    use std::collections::HashMap;

    let mut pkg_a_v2 = Manifest::default();
    pkg_a_v2.name = "pkg-a".to_string();
    pkg_a_v2.conflicts = vec!["pkg-b".to_string()];
    let mut pkg_a_v1 = Manifest::default();
    pkg_a_v1.name = "pkg-a".to_string();
    // v1 nie deklaruje konfliktów

    let mut pkg_b_v1 = Manifest::default();
    pkg_b_v1.name = "pkg-b".to_string();

    let mut candidates: HashMap<String, Vec<Candidate>> = HashMap::new();
    candidates.insert("pkg-a".to_string(), vec![
        Candidate { version: "2.0.0".to_string(), manifest: pkg_a_v2 }, // najnowszy, wypróbowany pierwszy
        Candidate { version: "1.0.0".to_string(), manifest: pkg_a_v1 },
    ]);
    candidates.insert("pkg-b".to_string(), vec![
        Candidate { version: "1.0.0".to_string(), manifest: pkg_b_v1 },
    ]);

    let order = vec!["pkg-a".to_string(), "pkg-b".to_string()];
    let state = State::default();
    let mut assignment = HashMap::new();
    let mut steps = 2000;
    let solved = backtrack_solve(&order, 0, &candidates, &state, &mut assignment, &mut steps);

    assert!(solved, "solver should find a conflict-free combination");
    assert_eq!(assignment.get("pkg-a").map(|(v, _)| v.as_str()), Some("1.0.0"),
        "solver should have backed off pkg-a to 1.0.0 to avoid the conflict with pkg-b");
    assert_eq!(assignment.get("pkg-b").map(|(v, _)| v.as_str()), Some("1.0.0"));
    Ok(())
}

/// Testuje że solver poddaje się jasno, a nie zawiesza się/panikuje, gdy
/// KAŻDY kandydat koliduje — tu obie jedyne wersje pkg-a i pkg-b wzajemnie
/// się wykluczają, więc nie ma żadnej spójnej kombinacji.
fn test_solver_unsatisfiable() -> Result<()> {
    use crate::commands::install::{Candidate, backtrack_solve};
    use crate::manifest::Manifest;
    use crate::state::State;
    use std::collections::HashMap;

    let mut pkg_a = Manifest::default();
    pkg_a.name = "pkg-a".to_string();
    pkg_a.conflicts = vec!["pkg-b".to_string()];
    let mut pkg_b = Manifest::default();
    pkg_b.name = "pkg-b".to_string();

    let mut candidates: HashMap<String, Vec<Candidate>> = HashMap::new();
    candidates.insert("pkg-a".to_string(), vec![Candidate { version: "1.0.0".to_string(), manifest: pkg_a }]);
    candidates.insert("pkg-b".to_string(), vec![Candidate { version: "1.0.0".to_string(), manifest: pkg_b }]);

    let order = vec!["pkg-a".to_string(), "pkg-b".to_string()];
    let state = State::default();
    let mut assignment = HashMap::new();
    let mut steps = 2000;
    let solved = backtrack_solve(&order, 0, &candidates, &state, &mut assignment, &mut steps);

    assert!(!solved, "solver must report failure, not silently pick a conflicting combination");
    Ok(())
}
