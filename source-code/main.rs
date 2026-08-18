mod error;
mod manifest;
mod sandbox;
mod state;
mod db;
mod squash;
mod repo;
mod commands;
mod utils;
mod hooks;

use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// --verbose / -v
//
// Global, crate-wide flag (not threaded through every function signature —
// that would mean touching dozens of call sites for a diagnostic feature).
// Set as early as possible in `main()`, below, from either position:
//   hpm --verbose install cosmic     (before the subcommand — works for
//   hpm -v install cosmic             EVERY subcommand automatically)
//   hpm install cosmic --verbose     (install-specific: see install.rs,
//                                      which filters --verbose/-v out of
//                                      its own args the same way it already
//                                      does for --release/--require-signed)
// Read from anywhere in the crate via `crate::is_verbose()`, or use the
// `crate::vlog!(...)` macro below for a one-line "print only if verbose".
// ---------------------------------------------------------------------------

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    if v {
        VERBOSE.store(true, Ordering::Relaxed);
    }
}

pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// `crate::vlog!("spawning {:?} with args {:?}", cmd, args)` — prints to
/// stderr, prefixed `[verbose]`, but ONLY when `--verbose`/`-v` was passed;
/// otherwise a no-op. Intentionally uncolored (unlike the rest of hpm's
/// output) so every call site works without needing `colored::Colorize`
/// in scope, and so verbose lines are trivially `grep`-able.
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::is_verbose() {
            eprintln!("[verbose] {}", format!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Lokalizacje danych hpm
//
// Od wersji 0.9 pakiety NIE są już instalowane systemowo w
// /usr/lib/HackerOS/hpm/store — hpm działa teraz w całości w przestrzeni
// użytkownika, bez roota:
//
//   ~/.hackeros/hpm/store/   — zainstalowane pakiety (dawniej STORE_PATH)
//   ~/.hackeros/hpm/cache/   — indeks repo (repo-list.json/repo.json) oraz
//                              pobrane archiwa .hpm (dawniej /var/cache/hpm)
//   ~/.hackeros/hpm/db/      — baza stanu (zainstalowane wersje, blokady,
//                              historia rollbacków) — patrz `state.rs`
//
// Zachowane jako funkcje (nie `const`), bo zależą od $HOME w czasie
// wykonania — każda wołana raz i cache'owana w `OnceLock`.
// ---------------------------------------------------------------------------

fn hackeros_home() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/root"))
        .join(".hackeros")
        .join("hpm")
}

pub fn store_path() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = hackeros_home().join("store");
        let _ = std::fs::create_dir_all(&p);
        format!("{}/", p.display())
    })
}

pub fn cache_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = hackeros_home().join("cache");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

pub fn db_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = hackeros_home().join("db");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

// Integracja z systemem (wrappery binarek, pliki .desktop, ikony) też
// przeniesiona do przestrzeni użytkownika — cały `hpm` (od 0.9) działa bez
// roota. Odpowiednik $HOME/.local/{bin,share/...} ze specyfikacji XDG.
pub fn bin_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join(".local").join("bin");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

pub fn desktop_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join(".local").join("share").join("applications");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

pub fn icon_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join(".local").join("share").join("icons").join("hicolor");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

pub fn pixmap_dir() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let p = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join(".local").join("share").join("pixmaps");
        let _ = std::fs::create_dir_all(&p);
        p.display().to_string()
    })
}

fn main() -> Result<()> {
    // UWAGA: to musi być zwykłe, pozycyjne parsowanie, NIE lexopt na całym
    // wektorze argumentów. Bug znaleziony przez realny test `hpm dev <path>
    // run <bin> --version`: lexopt przechwytywał `--version`/`--help`
    // WSZĘDZIE w linii poleceń jako flagi hpm, więc `hpm run pkg bin
    // --version` pokazywał wersję hpm zamiast przekazać `--version` do
    // opakowanej binarki. Flagi `-h`/`--help`/`-V`/`--version` liczą się
    // jako globalne TYLKO zanim ustalimy nazwę komendy — wszystko po niej
    // (włącznie z `-h`/`--version` należącymi do subkomendy albo do binarki,
    // którą subkomenda uruchamia) leci dalej bez zmian.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut command:  Option<String> = None;
    let mut sub_args: Vec<String>    = Vec::new();

    for arg in raw_args {
        if command.is_none() {
            match arg.as_str() {
                "-h" | "--help"    => { print_help(); return Ok(()); }
                "-V" | "--version" => { println!("hpm {}", env!("CARGO_PKG_VERSION")); return Ok(()); }
                "-v" | "--verbose" => { set_verbose(true); }
                _ => { command = Some(arg); }
            }
        } else {
            sub_args.push(arg);
        }
    }

    let command = command.unwrap_or_else(|| { print_help(); std::process::exit(0); });

    let result = match command.as_str() {
        "refresh"    => commands::refresh::refresh(),
        "install"    => commands::install::install(sub_args),
        "remove"     => commands::remove::remove(sub_args.first().cloned().unwrap_or_default()),
        "update"     => commands::update::update(),
        "switch"     => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm switch <package> <version>", "✗".red());
                std::process::exit(1);
            }
            commands::switch_version(sub_args[0].clone(), sub_args[1].clone())
        }
        "upgrade"    => commands::upgrade::upgrade(),
        "run"        => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm run <package> <bin> [args...]", "✗".red());
                std::process::exit(1);
            }
            let package = sub_args[0].clone();
            let bin     = sub_args[1].clone();
            let args    = sub_args[2..].to_vec();
            commands::run::run(package, bin, args)
        }
        "rollback"   => commands::rollback::rollback(sub_args.first().cloned()),
        "autoremove" => commands::autoremove::autoremove(),
        "search"     => commands::search::search(sub_args.first().cloned().unwrap_or_default()),
        "info"       => commands::info::info(sub_args.first().cloned().unwrap_or_default()),
        "list"       => commands::list::list_installed(),
        "outdated"   => commands::outdated::outdated(),
        "deps"       => commands::deps::deps(sub_args.first().cloned().unwrap_or_default()),
        "tags"       => cmd_tags(),
        "diff"       => commands::diff::diff(sub_args),
        "build"      => commands::build::build(sub_args),
        "clean"      => {
            // hpm clean         — czyści cache
            // hpm clean --all   — czyści cache + stare wersje ze store
            let all = sub_args.iter().any(|a| a == "--all");
            if all { commands::clean::clean_all() }
            else   { commands::clean::clean_cache() }
        }
        "verify"     => {
            if sub_args.first().map(|s| s.as_str()) == Some("--import-key") {
                let keyid = sub_args.get(1).cloned().unwrap_or_default();
                if keyid.is_empty() {
                    eprintln!("{} Usage: hpm verify --import-key <keyid|keyfile|url>", "✗".red());
                    std::process::exit(1);
                }
                commands::verify::import_key(&keyid)
            } else {
                commands::verify::verify(sub_args.first().cloned().unwrap_or_default())
            }
        }
        "pin"        => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm pin <package> <version>", "✗".red());
                std::process::exit(1);
            }
            commands::pin::pin(sub_args[0].clone(), sub_args[1].clone())
        }
        "unpin"      => commands::unpin::unpin(sub_args.first().cloned().unwrap_or_default()),
        "__debug_hash" => {
            let dir = sub_args.first().cloned().unwrap_or_default();
            let h = crate::utils::compute_dir_hash(std::path::Path::new(&dir))?;
            println!("{}", h);
            Ok(())
        }
        "__debug_gpg" => {
            let data = sub_args.first().cloned().unwrap_or_default();
            let sig  = sub_args.get(1).cloned().unwrap_or_default();
            match commands::verify::verify_gpg_signature(
                std::path::Path::new(&data), std::path::Path::new(&sig)
            ) {
                Ok(signer) => { println!("OK: {}", signer); Ok(()) }
                Err(e)     => { println!("FAIL: {}", e); Ok(()) }
            }
        }
        "doctor"     => commands::doctor::doctor(),
        "repair"     => commands::repair::repair(),
        "lock"       => commands::lock::lock(sub_args),
        "create"     => commands::create::create(sub_args.first().cloned()),
        // Ukryta komenda dev — nie w --help
        "dev"        => commands::dev::dev(sub_args),
        _ => {
            eprintln!("{} Unknown command: {}", "✗".red(), command);
            print_help();
            std::process::exit(1);
        }
    };

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            // ── NOWE: sudo hint przy Permission denied ────────────────────────
            // Jeśli błąd zawiera "Permission denied" lub "os error 13"
            // — pokaż przyjazną sugestię zamiast surowego błędu
            let err_str = e.to_string();
            if is_permission_error(&err_str) {
                print_sudo_hint(&command);
            } else {
                eprintln!("{} {}", "✗".red(), e);
            }
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Permission denied detection and sudo hint
// ---------------------------------------------------------------------------

fn is_permission_error(err: &str) -> bool {
    err.contains("Permission denied")
        || err.contains("os error 13")
        || err.contains("EACCES")
        || err.contains("permission denied")
}

fn print_sudo_hint(command: &str) {
    println!();
    println!("{} {}", "✗".red().bold(), "Permission denied".red().bold());
    println!();

    // Od 0.9 hpm działa WYŁĄCZNIE w przestrzeni użytkownika
    // (~/.hackeros/hpm/{store,cache,db} + ~/.local/{bin,share/...}) — żadna
    // z komend nie wymaga już roota. `sudo hpm ...` jest teraz ZŁYM
    // pomysłem: root ma inny $HOME, więc `sudo` rozjechałby store między
    // /root/.hackeros a katalogiem prawdziwego użytkownika. Ten błąd
    // najczęściej oznacza, że wcześniej coś (np. stara wersja hpm sprzed
    // 0.9, albo ręczne `sudo`) już utworzyło te katalogi jako root.
    let _ = command; // zachowane w sygnaturze na wypadek przyszłego zróżnicowania per-komenda
    println!("  {} hpm no longer needs root — packages live under your own home directory:",
             "ℹ".white());
    println!("      {}", crate::store_path().dimmed());
    println!("      {}", crate::bin_dir().dimmed());
    println!();
    println!("  {} This usually means those directories (or their parent ~/.hackeros / ~/.local)",
             "→".bright_black());
    println!("    are owned by someone else — often because they were created by root");
    println!("    (e.g. a stray `sudo hpm ...` before 0.9, or a previous install as another user).");
    println!();
    println!("  Fix ownership, then retry without sudo:");
    println!();
    println!("    {}", format!("sudo chown -R $USER: ~/.hackeros ~/.local/bin ~/.local/share").bold().white());
    println!();
}

// ---------------------------------------------------------------------------
// hpm tags
// ---------------------------------------------------------------------------

fn cmd_tags() -> Result<()> {
    let repo_mgr = crate::repo::RepoManager::load_sync()?;
    let tags     = repo_mgr.all_tags();
    if tags.is_empty() {
        println!("{} No group tags found.", "→".bright_black());
        println!("  Tags are defined in each package's {} file.", "info.hk".bright_black());
        println!("  Run {} to fetch metadata from all packages.", "hpm refresh".bright_black());
        return Ok(());
    }
    println!("{} Available group tags:\n", "→".white());
    for tag in &tags {
        let pkgs  = repo_mgr.packages_for_tag(tag);
        let count = pkgs.len();
        let preview: Vec<&str> = pkgs.iter().take(5).map(|p| p.as_str()).collect();
        let suffix = if count > 5 { format!(" +{} more", count - 5) } else { String::new() };
        println!("  {} {:20} {} package(s): {}{}",
            "◆".white(),
            format!("@{}", tag).red(),
            count,
            preview.join(", ").dimmed(),
            suffix.dimmed()
        );
    }
    println!();
    println!("  Install a tag group : {}", "hpm install @<tag>".bright_black());
    println!("  Search by tag       : {}", "hpm search @<tag>".bright_black());
    Ok(())
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!("\n{} {}\n", "Hacker Package Manager (hpm)".bold().red(), version.red());
    println!("{}  hpm {} [options]\n", "Usage:".bold(), "<command>".bright_black());

    println!("{}", "Package Commands:".bold().underline());
    println!("  {:<38} {}", "refresh".red(),                    "Update index and pre-fetch metadata");
    println!("  {:<38} {}", "install <pkg>[@<ver>]...".red(),   "Install packages (no root needed)");
    println!("  {:<38} {}", "install <pkg> --release".red(),    "Install from a GitHub Release .hpm instead of git clone");
    println!("  {:<38} {}", "install <pkg> --release --require-signed".red(), "...and refuse if it isn't GPG-signed");
    println!("  {:<38} {}", "install @<tag>".red(),             "Install all packages with group tag");
    println!("  {:<38} {}", "remove <pkg>[@<ver>]".red(),       "Remove package");
    println!("  {:<38} {}", "autoremove".red(),                 "Remove orphaned packages");
    println!("  {:<38} {}", "update".red(),                     "Update all packages");
    println!("  {:<38} {}", "upgrade".red(),                    "Upgrade hpm itself");
    println!("  {:<38} {}", "switch <pkg> <ver>".red(),         "Switch active version");
    println!("  {:<38} {}", "rollback [<pkg>]".red(),           "Restore previous state");

    println!();
    println!("{}", "Query Commands:".bold().underline());
    println!("  {:<38} {}", "search <query|@tag>".red(),        "Search packages");
    println!("  {:<38} {}", "info <package>".red(),             "Show package details");
    println!("  {:<38} {}", "list".red(),                       "List installed packages");
    println!("  {:<38} {}", "outdated".red(),                   "Show packages with updates");
    println!("  {:<38} {}", "deps <pkg>[@<ver>]".red(),         "Show dependency tree");
    println!("  {:<38} {}", "tags".red(),                       "List available group tags");
    println!("  {:<38} {}", "diff <pkg> <v1> [<v2>]".red(),    "Compare two package versions");

    println!();
    println!("{}", "Maintenance Commands:".bold().underline());
    println!("  {:<38} {}", "run <pkg> <bin> [args]".red(),     "Run binary (sandboxed)");
    println!("  {:<38} {}", "build [name]".red(),               "Package current directory into a .hpm archive");
    println!("  {:<38} {}", "build --output <path>".red(),      "Custom output path for the .hpm archive");
    println!("  {:<38} {}", "build --sign <key-id>".red(),      "Also produce a detached GPG .sig (like pacman)");
    println!("  {:<38} {}", "clean".red(),                      "Remove cached repos + temp files");
    println!("  {:<38} {}", "clean --all".red(),                "Also remove old store versions");
    println!("  {:<38} {}", "verify <package>".red(),           "Verify SHA-256 + GPG signature");
    println!("  {:<38} {}", "verify --import-key <key>".red(),  "Import GPG key to trusted keyring");
    println!("  {:<38} {}", "pin <pkg> <ver>".red(),            "Pin a package version");
    println!("  {:<38} {}", "unpin <pkg>".red(),                "Unpin current version");
    println!("  {:<38} {}", "doctor".red(),                     "Diagnose consistency issues");
    println!("  {:<38} {}", "repair".red(),                     "Auto-fix issues found by doctor");
    println!("  {:<38} {}", "lock <subcmd>".red(),              "Manage hpm.lock (reproducible installs)");

    println!();
    println!("{}", "Development Commands:".bold().underline());
    println!("  {:<38} {}", "create [<name>]".red(),            "Interactive package creation wizard");
    println!("  {:<38} {}", "dev <path>".red(),                 "Test a local package dir (not in repo-list.json)");
    println!("  {:<38} {}", "dev <path> run <bin> [args]".red(),"...and run one of its binaries, sandboxed");

    println!();
    println!("{}", "Options:".bold().underline());
    println!("  {}, {:<28} {}", "-h".bright_black(), "--help".bright_black(),    "Show this help");
    println!("  {}, {:<28} {}", "-V".bright_black(), "--version".bright_black(), "Show version");
    println!("  {}, {:<28} {}", "-v".bright_black(), "--verbose".bright_black(),
        "Print diagnostic detail (spawned commands, paths, ...); before or after the subcommand");

    println!();
    println!("{}", "Group tags:".bold().underline());
    println!("  {}  →  install all @development packages", "hpm install @development".bright_black());
    println!("  {}  →  list all available tags",           "hpm tags".bright_black());

    println!();
    println!("{}", "Hooks (Hacker Lang):".bold().underline());
    println!("  Package hooks are placed in {} subdirectory:", "hooks/".white());
    println!("  {:<30} {}", "pre-install.hl".white(),  "runs before install (blocks on failure)");
    println!("  {:<30} {}", "post-install.hl".white(), "runs after install");
    println!("  {:<30} {}", "pre-remove.hl".white(),   "runs before removal (blocks on failure)");
    println!("  {:<30} {}", "post-remove.hl".white(),  "runs after removal");
    println!("  {:<30} {}", "post-update.hl".white(),  "runs after update");
    println!("  Also supported: {} {} {}", ".py".dimmed(), ".rb".dimmed(), ".sh".dimmed());

    println!();
    println!("{}", "Package format:".bold().underline());
    println!("  info.hk          Manifest (name, version, bins, deps, sandbox, tags)");
    println!("  build.toml       Build/download instructions (optional)");
    println!("  contents/        Pre-built files");
    println!("  hooks/           Hacker Lang hooks (.hl) — pre/post install/remove");
    println!("  info.hk.sig      GPG signature (optional)");
    println!();
    println!("  {} hpm runs entirely as your user — {} is only needed", "ℹ".white(), "no sudo".red().bold());
    println!("    if {} or {} are owned by root from before 0.9.", crate::store_path().dimmed(), crate::bin_dir().dimmed());
    println!();
}
