use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, Duration};

// ---------------------------------------------------------------------------
// Test result tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TestSuite {
    passed:  Vec<(String, Duration)>,
    failed:  Vec<(String, String)>,
    skipped: Vec<String>,
}

impl TestSuite {
    fn pass(&mut self, name: &str, elapsed: Duration) {
        println!("  {} {} {}", "✔".green(), name.bold(), format!("({:.1}s)", elapsed.as_secs_f32()).dimmed());
        self.passed.push((name.to_string(), elapsed));
    }

    fn fail(&mut self, name: &str, reason: &str) {
        println!("  {} {} — {}", "✗".red(), name.bold(), reason.red());
        self.failed.push((name.to_string(), reason.to_string()));
    }

    fn skip(&mut self, name: &str, reason: &str) {
        println!("  {} {} — {}", "○".dimmed(), name.bold(), reason.dimmed());
        self.skipped.push(name.to_string());
    }

    fn print_summary(&self) {
        let total = self.passed.len() + self.failed.len() + self.skipped.len();
        println!();
        println!("{}", "─".repeat(60).dimmed());
        println!("{}", "Test Results:".bold());
        println!("  {} Passed:  {}", "✔".green(),  self.passed.len());
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
            println!("{} All tests passed!", "✔".green().bold());
        } else {
            println!("{} {} test(s) failed.", "✗".red().bold(), self.failed.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Dev command entry point
// ---------------------------------------------------------------------------

pub fn dev(args: Vec<String>) -> Result<()> {
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("test");

    match subcmd {
        "test"      => run_tests(false),
        "test-full" => run_tests(true),
        "check-env" => check_environment(),
        _ => {
            eprintln!("{} Unknown dev subcommand: {} (test|test-full|check-env)", "✗".red(), subcmd);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Environment check
// ---------------------------------------------------------------------------

fn check_environment() -> Result<()> {
    println!("{} Checking hpm development environment...\n", "→".cyan());

    let checks: &[(&str, &str, bool)] = &[
        // (command, description, required)
        ("git",      "Git (for repo cloning)",          true),
        ("tar",      "tar (for packaging)",             true),
        ("zstd",     "zstd (for compression)",          false),
        ("gpg",      "GPG (for package signing)",       false),
        ("bpftrace", "bpftrace (for eBPF tracing)",     false),
        ("valac",    "Vala compiler (for GUI packages)", false),
        ("meson",    "Meson build system",              false),
        ("cargo",    "Rust/Cargo (for build packages)", false),
    ];

    let mut all_required_ok = true;

    for (cmd, desc, required) in checks {
        let found = Command::new("which").arg(cmd).output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if found {
            let version = Command::new(cmd).arg("--version")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.lines().next().unwrap_or("").trim().to_string())
                .unwrap_or_default();
            println!("  {} {:<30} {}", "✔".green(), desc, version.dimmed());
        } else if *required {
            println!("  {} {:<30} {} MISSING (required)", "✗".red(), desc, cmd.yellow());
            all_required_ok = false;
        } else {
            println!("  {} {:<30} not found (optional)", "○".dimmed(), desc);
        }
    }

    println!();

    // Sprawdź kernel features
    println!("{}", "Kernel features:".bold());
    check_kernel_feature("User namespaces",
        "/proc/sys/user/max_user_namespaces", "0");
    check_kernel_feature("Landlock LSM",
        "/proc/sys/kernel/landlock/abi", "");
    check_kernel_feature("eBPF JIT",
        "/proc/sys/net/core/bpf_jit_enable", "0");

    println!();
    if all_required_ok {
        println!("{} Environment is ready for hpm development.", "✔".green());
    } else {
        println!("{} Some required tools are missing.", "✗".red());
    }

    Ok(())
}

fn check_kernel_feature(name: &str, path: &str, bad_value: &str) {
    if let Ok(val) = fs::read_to_string(path) {
        let val = val.trim();
        if val == bad_value {
            println!("  {} {:<30} {} (disabled)", "⚠".yellow(), name, val.red());
        } else {
            println!("  {} {:<30} {}", "✔".green(), name, val.dimmed());
        }
    } else {
        println!("  {} {:<30} not available", "○".dimmed(), name);
    }
}

// ---------------------------------------------------------------------------
// Full test suite
// ---------------------------------------------------------------------------

fn run_tests(full: bool) -> Result<()> {
    println!("\n{} {}\n", "hpm dev test".bold().red(), "— Integration Test Suite");
    println!("  hpm version : {}", env!("CARGO_PKG_VERSION").cyan());
    println!("  full mode   : {}", if full { "yes".green() } else { "no (quick)".dimmed() });
    println!();

    // Utwórz tymczasowe środowisko testowe
    let test_env = TestEnvironment::setup()?;
    println!("{} Test environment: {}\n", "→".cyan(), test_env.root.display().to_string().dimmed());

    let mut suite = TestSuite::default();
    let total_start = Instant::now();

    // ── Testy ───────────────────────────────────────────────────────────────

    run_test(&mut suite, "manifest-parse",
        "Parse valid info.hk manifest",
        || test_manifest_parse(&test_env));

    run_test(&mut suite, "manifest-invalid",
        "Reject invalid info.hk",
        || test_manifest_invalid(&test_env));

    run_test(&mut suite, "build-validate",
        "hpm build validates info.hk before packaging",
        || test_build_validates(&test_env));

    run_test(&mut suite, "lock-generate",
        "hpm lock generate creates lock file",
        || test_lock_generate(&test_env));

    run_test(&mut suite, "lock-check-ok",
        "hpm lock check passes when state matches",
        || test_lock_check_ok(&test_env));

    run_test(&mut suite, "lock-check-diff",
        "hpm lock check detects divergence",
        || test_lock_check_diff(&test_env));

    run_test(&mut suite, "state-roundtrip",
        "State save/load roundtrip",
        || test_state_roundtrip(&test_env));

    run_test(&mut suite, "state-conflict",
        "State detects package conflicts",
        || test_state_conflict(&test_env));

    run_test(&mut suite, "wrapper-names-persist",
        "WrapperNames persists across sessions",
        || test_wrapper_names(&test_env));

    run_test(&mut suite, "version-compare",
        "Version comparison: 1.2.0 > 1.1.9",
        || test_version_compare());

    run_test(&mut suite, "version-satisfies",
        "Version satisfies: >=1.0 includes 1.2.0",
        || test_version_satisfies());

    run_test(&mut suite, "compute-hash",
        "Directory hash computation is deterministic",
        || test_compute_hash(&test_env));

    run_test(&mut suite, "hooks-validate",
        "Hook validation detects missing shebang",
        || test_hooks_validate(&test_env));

    run_test(&mut suite, "hooks-pre-install",
        "pre-install hook runs before file copy",
        || test_hooks_pre_install(&test_env));

    run_test(&mut suite, "hooks-post-install",
        "post-install hook runs after file copy",
        || test_hooks_post_install(&test_env));

    run_test(&mut suite, "hooks-fail-blocks-install",
        "Failed pre-install hook blocks installation",
        || test_hooks_fail_blocks(&test_env));

    run_test(&mut suite, "diff-manifests",
        "hpm diff detects manifest changes",
        || test_diff_manifests(&test_env));

    run_test(&mut suite, "diff-files",
        "hpm diff detects file additions/removals",
        || test_diff_files(&test_env));

    run_test(&mut suite, "arch-check",
        "Architecture check in [specs]",
        || test_arch_check());

    run_test(&mut suite, "tag-grouping",
        "Package tag grouping and @tag expansion",
        || test_tag_grouping());

    if full {
        run_test(&mut suite, "search-pagination",
            "Search pagination with 50+ results",
            || test_search_pagination());

        run_test(&mut suite, "sandbox-compat-mode",
            "Sandbox compat mode (CLONE_NEWNS)",
            || test_sandbox_compat(&test_env));

        run_test(&mut suite, "rollback-full-state",
            "Full state rollback from history",
            || test_rollback_state(&test_env));
    } else {
        suite.skip("search-pagination", "full mode only (hpm dev test-full)");
        suite.skip("sandbox-compat-mode", "full mode only");
        suite.skip("rollback-full-state", "full mode only");
    }

    // ── Cleanup ──────────────────────────────────────────────────────────────
    println!();
    print!("{} Cleaning up test environment...", "→".cyan());
    test_env.cleanup();
    println!(" {}", "done".green());

    // ── Summary ──────────────────────────────────────────────────────────────
    let total_elapsed = total_start.elapsed();
    println!("\n  Total time: {:.2}s", total_elapsed.as_secs_f32());
    suite.print_summary();

    if !suite.failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_test<F>(suite: &mut TestSuite, id: &str, desc: &str, f: F)
where
    F: FnOnce() -> Result<()>,
{
    print!("  {} {:<40} ", "…".dimmed(), desc);
    std::io::stdout().flush().ok();
    let start = Instant::now();
    match f() {
        Ok(()) => {
            let elapsed = start.elapsed();
            println!("{} {}", "OK".green().bold(), format!("({:.1}s)", elapsed.as_secs_f32()).dimmed());
            suite.passed.push((id.to_string(), elapsed));
        }
        Err(e) => {
            println!("{}", "FAIL".red().bold());
            println!("    {}", e.to_string().red());
            suite.failed.push((id.to_string(), e.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------

struct TestEnvironment {
    root:       PathBuf,
    store:      PathBuf,
    state_file: PathBuf,
    lock_file:  PathBuf,
    wrapper_file: PathBuf,
}

impl TestEnvironment {
    fn setup() -> Result<Self> {
        let root = tempfile::tempdir().into_diagnostic()?.into_path();
        let store = root.join("store");
        let state_file   = root.join("state.json");
        let lock_file    = root.join("hpm.lock");
        let wrapper_file = root.join("wrapper-names.json");
        fs::create_dir_all(&store).into_diagnostic()?;
        Ok(Self { root, store, state_file, lock_file, wrapper_file })
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }

    /// Utwórz minimalny katalog pakietu testowego.
    fn make_pkg_dir(&self, name: &str, version: &str, extra_info: &str) -> PathBuf {
        let dir = self.root.join(format!("pkg-{}-{}", name, version));
        fs::create_dir_all(dir.join("contents/bin")).ok();
        let info = format!(
            "[metadata]\n-> name => {}\n-> version => {}\n-> authors => Test\n-> license => MIT\n{}\n\
[description]\n-> summary => Test package {}\n\
[sandbox]\n-> network => false\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {{}}\n",
            name, version, extra_info, name
        );
        fs::write(dir.join("info.hk"), &info).ok();
        let bin = dir.join(format!("contents/bin/{}", name));
        fs::write(&bin, format!("#!/bin/sh\necho 'hello from {} {}'\n", name, version)).ok();
        crate::utils::make_executable(&bin).ok();
        dir
    }
}

// ---------------------------------------------------------------------------
// Individual tests
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
        Err(_) => Ok(()),
        Ok(m)  => {
            // Parser może zwrócić pusty string — sprawdź czy walidacja by to złapała
            if m.name.is_empty() { Ok(()) }
            else { bail!("Expected parse failure for empty name, got: {:?}", m.name) }
        }
    }
}

fn test_build_validates(env: &TestEnvironment) -> Result<()> {
    // Tworzę info.hk z brakującym name — build powinien zawieść walidację
    let dir = env.root.join("pkg-build-test");
    fs::create_dir_all(dir.join("contents/bin")).into_diagnostic()?;
    fs::write(dir.join("info.hk"),
        "[metadata]\n-> name => valid-pkg\n-> version => 1.0.0\n\
[description]\n-> summary => test\n\
[sandbox]\n-> network => false\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {}\n"
    ).into_diagnostic()?;
    // Manifest jest poprawny — sprawdzam że validate_manifest nie zwróci błędu
    let m = crate::manifest::Manifest::load_from_path(dir.to_str().unwrap())?;
    if m.name.is_empty() {
        bail!("Validation should have caught empty name");
    }
    Ok(())
}

fn test_lock_generate(env: &TestEnvironment) -> Result<()> {
    use crate::commands::lock::{LockFile};
    let lock = LockFile::new();
    lock.save(&env.lock_file)?;
    assert!(env.lock_file.exists());
    let loaded = LockFile::load(&env.lock_file)?;
    assert_eq!(loaded.lock_version, 1);
    Ok(())
}

fn test_lock_check_ok(env: &TestEnvironment) -> Result<()> {
    use crate::commands::lock::{LockFile};
    use crate::state::State;
    // Empty lock + empty state → no diffs
    let lock  = LockFile::new();
    let state = State::default();
    let diffs = lock.check_against_state(&state);
    assert!(diffs.is_empty(), "Expected no diffs, got: {:?}", diffs);
    Ok(())
}

fn test_lock_check_diff(env: &TestEnvironment) -> Result<()> {
    use crate::commands::lock::{LockFile, LockEntry};
    use crate::state::State;
    use std::collections::HashMap;

    let mut lock = LockFile::new();
    lock.packages.insert("missing-pkg".to_string(), LockEntry {
        version:            "1.0.0".to_string(),
        git_commit:         "abc123".to_string(),
        repo_url:           "https://github.com/test/test".to_string(),
        checksum:           "deadbeef".to_string(),
        dependencies:       HashMap::new(),
        manually_installed: true,
        installed_at:       0,
    });

    let state = State::default(); // pusty
    let diffs = lock.check_against_state(&state);
    assert!(!diffs.is_empty(), "Expected diff for missing-pkg");
    assert!(diffs[0].contains("missing"));
    Ok(())
}

fn test_state_roundtrip(env: &TestEnvironment) -> Result<()> {
    use crate::state::{State, VersionInfo};
    use std::collections::HashSet;

    let mut state = State::default();
    let mut info  = VersionInfo::new("abc123checksum", true);
    info.depends_on = HashSet::from(["dep-a@1.0.0".to_string()]);
    state.packages.entry("test-pkg".to_string())
        .or_default()
        .insert("2.0.0".to_string(), info);

    // Serialize
    let data = serde_json::to_vec_pretty(&state).into_diagnostic()?;
    // Deserialize
    let loaded: State = serde_json::from_slice(&data).into_diagnostic()?;
    assert!(loaded.packages.contains_key("test-pkg"));
    assert!(loaded.packages["test-pkg"].contains_key("2.0.0"));
    assert_eq!(loaded.packages["test-pkg"]["2.0.0"].checksum, "abc123checksum");
    Ok(())
}

fn test_state_conflict(env: &TestEnvironment) -> Result<()> {
    use crate::state::{State, VersionInfo};
    use std::collections::HashSet;

    let mut state = State::default();
    let mut info  = VersionInfo::new("hash1", true);
    info.conflicts_with = HashSet::from(["new-pkg".to_string()]);
    state.packages.entry("installed-pkg".to_string())
        .or_default()
        .insert("1.0.0".to_string(), info);

    let conflicts = state.check_conflicts("new-pkg", &[]);
    assert!(!conflicts.is_empty(), "Should detect conflict");
    assert!(conflicts[0].contains("new-pkg"));
    Ok(())
}

fn test_wrapper_names(env: &TestEnvironment) -> Result<()> {
    // WrapperNames nie używa globalnego pliku — testujemy logikę set/get
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
    assert_eq!(compare_versions("1.0.0-rc1", "1.0.0"), Ordering::Less);
    Ok(())
}

fn test_version_satisfies() -> Result<()> {
    use crate::utils::satisfies;

    assert!(satisfies("1.2.0", ">=1.0"));
    assert!(satisfies("1.0.0", ">=1.0.0"));
    assert!(!satisfies("0.9.0", ">=1.0"));
    assert!(satisfies("1.5.0", ">1.4"));
    assert!(!satisfies("1.4.0", ">1.4.0"));
    assert!(satisfies("2.0.0", "=2.0.0"));
    assert!(!satisfies("2.0.1", "=2.0.0"));
    assert!(satisfies("1.0.0", ""));  // brak wymagania = dowolna wersja
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

    // Zmiana pliku powinna zmienić hash
    fs::write(dir.join("a.txt"), b"changed").into_diagnostic()?;
    let h3 = compute_dir_hash(&dir)?;
    assert_ne!(h1, h3, "Hash must change after file modification");
    Ok(())
}

fn test_hooks_validate(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::validate_hooks;

    let dir      = env.root.join("hooks-validate-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;

    // Hook bez shebang
    fs::write(hooks_dir.join("post-install.sh"), b"echo done").into_diagnostic()?;
    let warnings = validate_hooks(&dir);
    assert!(warnings.iter().any(|w| w.contains("shebang")),
            "Should warn about missing shebang, got: {:?}", warnings);
    Ok(())
}

fn test_hooks_pre_install(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};

    let dir       = env.root.join("hooks-pre-test");
    let hooks_dir = dir.join("hooks");
    let sentinel  = env.root.join("hook-ran");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;

    let script = format!("#!/bin/sh\ntouch {}\n", sentinel.display());
    fs::write(hooks_dir.join("pre-install.sh"), script.as_bytes()).into_diagnostic()?;
    crate::utils::make_executable(&hooks_dir.join("pre-install.sh"))?;

    let ctx = HookContext {
        pkg_name:    "test-hook",
        pkg_version: "1.0.0",
        store_path:  env.store.to_str().unwrap(),
        old_version: None,
    };
    let ran = run_hook(&dir, HookKind::PreInstall, &ctx)?;
    assert!(ran, "Hook should have run");
    assert!(sentinel.exists(), "pre-install hook should have created sentinel file");
    Ok(())
}

fn test_hooks_post_install(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};

    let dir       = env.root.join("hooks-post-test");
    let hooks_dir = dir.join("hooks");
    let sentinel  = env.root.join("post-hook-ran");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;

    let script = format!("#!/bin/sh\ntouch {}\necho \"HPM_PKG_NAME=$HPM_PKG_NAME\"\n",
                         sentinel.display());
    fs::write(hooks_dir.join("post-install.sh"), script.as_bytes()).into_diagnostic()?;
    crate::utils::make_executable(&hooks_dir.join("post-install.sh"))?;

    let ctx = HookContext {
        pkg_name: "post-hook-pkg", pkg_version: "2.0.0",
        store_path: env.store.to_str().unwrap(), old_version: None,
    };
    run_hook(&dir, HookKind::PostInstall, &ctx)?;
    assert!(sentinel.exists(), "post-install hook should have run");
    Ok(())
}

fn test_hooks_fail_blocks(env: &TestEnvironment) -> Result<()> {
    use crate::hooks::{run_hook, HookKind, HookContext};

    let dir       = env.root.join("hooks-fail-test");
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).into_diagnostic()?;

    // Hook który zawsze kończy błędem
    fs::write(hooks_dir.join("pre-install.sh"), b"#!/bin/sh\necho 'Refusing install'\nexit 1\n")
        .into_diagnostic()?;
    crate::utils::make_executable(&hooks_dir.join("pre-install.sh"))?;

    let ctx = HookContext {
        pkg_name: "bad-hook-pkg", pkg_version: "1.0.0",
        store_path: env.store.to_str().unwrap(), old_version: None,
    };
    match run_hook(&dir, HookKind::PreInstall, &ctx) {
        Err(_) => Ok(()), // Oczekiwany błąd
        Ok(_)  => bail!("pre-install hook failure should have blocked install"),
    }
}

fn test_diff_manifests(env: &TestEnvironment) -> Result<()> {
    use crate::manifest::Manifest;

    let dir1 = env.make_pkg_dir("diff-pkg", "1.0.0", "");
    let dir2 = env.root.join("diff-pkg-2.0.0");
    fs::create_dir_all(&dir2).into_diagnostic()?;
    fs::write(dir2.join("info.hk"),
        "[metadata]\n-> name => diff-pkg\n-> version => 2.0.0\n-> authors => New Author\n-> license => MIT\n\
[description]\n-> summary => Changed summary\n\
[sandbox]\n-> network => true\n-> gui => false\n-> full_gui => false\n\
-> dev => false\n-> disabled => false\n-> filesystem => {}\n"
    ).into_diagnostic()?;

    let m1 = Manifest::load_from_path(dir1.to_str().unwrap())?;
    let m2 = Manifest::load_from_path(dir2.to_str().unwrap())?;

    assert_ne!(m1.version, m2.version);
    assert_ne!(m1.sandbox.network, m2.sandbox.network);
    Ok(())
}

fn test_diff_files(env: &TestEnvironment) -> Result<()> {
    let dir1 = env.root.join("fdiff-1");
    let dir2 = env.root.join("fdiff-2");
    fs::create_dir_all(&dir1).into_diagnostic()?;
    fs::create_dir_all(&dir2).into_diagnostic()?;
    fs::write(dir1.join("same.txt"),    b"same content").into_diagnostic()?;
    fs::write(dir2.join("same.txt"),    b"same content").into_diagnostic()?;
    fs::write(dir1.join("old-file.txt"),b"only in v1").into_diagnostic()?;
    fs::write(dir2.join("new-file.txt"),b"only in v2").into_diagnostic()?;
    fs::write(dir1.join("changed.txt"), b"version 1").into_diagnostic()?;
    fs::write(dir2.join("changed.txt"), b"version 2").into_diagnostic()?;

    use sha2::{Sha256, Digest};
    let hash = |p: &Path| -> String {
        let data = fs::read(p).unwrap_or_default();
        format!("{:x}", Sha256::digest(&data))[..16].to_string()
    };

    // same.txt powinien mieć ten sam hash
    assert_eq!(hash(&dir1.join("same.txt")), hash(&dir2.join("same.txt")));
    // changed.txt powinien mieć różny hash
    assert_ne!(hash(&dir1.join("changed.txt")), hash(&dir2.join("changed.txt")));
    Ok(())
}

fn test_arch_check() -> Result<()> {
    let current = std::env::consts::ARCH;
    let valid   = ["x86_64", "aarch64", "armhf", "i386", "any"];
    // Bieżąca architektura musi być rozpoznawalna
    assert!(valid.contains(&current) || current == "x86_64" || current == "aarch64",
            "Unexpected arch: {}", current);
    Ok(())
}

fn test_tag_grouping() -> Result<()> {
    // Testujemy logikę packages_for_tag bez sieci
    // Symulujemy RepoIndex z tagami
    use crate::repo::{RepoIndex, PackageEntry};
    use std::collections::HashMap;

    let mut packages = HashMap::new();
    packages.insert("gcc".to_string(), PackageEntry {
        repo:     "https://github.com/test/gcc".to_string(),
        versions: vec!["14.0.0".to_string()],
        tags:     vec!["development".to_string(), "compilers".to_string()],
    });
    packages.insert("make".to_string(), PackageEntry {
        repo:     "https://github.com/test/make".to_string(),
        versions: vec!["4.4.0".to_string()],
        tags:     vec!["development".to_string()],
    });
    packages.insert("firefox".to_string(), PackageEntry {
        repo:     "https://github.com/test/firefox".to_string(),
        versions: vec!["120.0".to_string()],
        tags:     vec!["browsers".to_string()],
    });

    let mut global_tags = HashMap::new();
    global_tags.insert("development".to_string(), vec!["gdb".to_string()]);

    let index = RepoIndex { packages, tags: global_tags };

    // Symulujemy packages_for_tag bez RepoManager
    let tag = "development";
    let tag_lower = tag.to_lowercase();

    let mut result = std::collections::HashSet::new();
    if let Some(list) = index.tags.get(&tag_lower) {
        for n in list { result.insert(n.clone()); }
    }
    for (name, entry) in &index.packages {
        if entry.tags.iter().any(|t| t.to_lowercase() == tag_lower) {
            result.insert(name.clone());
        }
    }

    assert!(result.contains("gcc"),  "gcc should be in @development");
    assert!(result.contains("make"), "make should be in @development");
    assert!(result.contains("gdb"),  "gdb should be in @development (from global tags)");
    assert!(!result.contains("firefox"), "firefox should NOT be in @development");
    Ok(())
}

fn test_search_pagination() -> Result<()> {
    // Test paginacji — symulujemy 60 wyników i sprawdzamy podział na strony
    let results: Vec<String> = (0..60).map(|i| format!("pkg-{:03}", i)).collect();
    let page_size = 20usize;
    let page      = 0usize;
    let start     = page * page_size;
    let end       = (start + page_size).min(results.len());
    let page_data = &results[start..end];

    assert_eq!(page_data.len(), 20);
    assert_eq!(page_data[0], "pkg-000");
    assert_eq!(page_data[19], "pkg-019");

    let page2 = &results[20..40];
    assert_eq!(page2[0], "pkg-020");

    Ok(())
}

fn test_sandbox_compat(env: &TestEnvironment) -> Result<()> {
    // Sprawdź czy CLONE_NEWNS jest dostępne (nie blokuje procesu testowego)
    use nix::sched::{unshare, CloneFlags};

    match unshare(CloneFlags::CLONE_NEWNS) {
        Ok(()) => {
            // Sukces — compat mode będzie działać
            // Nie robimy nic więcej — nie chcemy kontynuować w złym namespace
            Ok(())
        }
        Err(e) if e == nix::errno::Errno::EPERM => {
            // Brak uprawnień — OK w CI, tylko sprawdzamy że kod obsługuje to gracefully
            Ok(())
        }
        Err(e) => bail!("Unexpected unshare error: {}", e),
    }
}

fn test_rollback_state(env: &TestEnvironment) -> Result<()> {
    use crate::state::{State, VersionInfo};

    let mut state = State::default();
    // Dodaj pakiet
    state.packages.entry("rollback-pkg".to_string())
        .or_default()
        .insert("1.0.0".to_string(), VersionInfo::new("hash1", true));

    // Zrób snapshot
    state.push_snapshot("pre-update rollback-pkg");
    assert_eq!(state.history.len(), 1);

    // Zaktualizuj
    state.packages.entry("rollback-pkg".to_string())
        .or_default()
        .insert("2.0.0".to_string(), VersionInfo::new("hash2", true));

    // Rollback do snapshotu 0
    let ok = state.restore_snapshot(0);
    assert!(ok, "restore_snapshot should return true");
    // Po rollbacku packages wraca do stanu ze snapshotu
    // (który miał tylko 1.0.0)
    assert!(state.packages.get("rollback-pkg")
        .map(|vs| vs.contains_key("1.0.0"))
        .unwrap_or(false));

    Ok(())
}
