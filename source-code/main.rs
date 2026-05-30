mod error;
mod manifest;
mod sandbox;
mod state;
mod repo;
mod commands;
mod utils;
mod hooks;

use lexopt::prelude::*;
use miette::{Result, IntoDiagnostic};
use colored::Colorize;

pub const STORE_PATH: &str = "/usr/lib/HackerOS/hpm/store/";
pub const CACHE_DIR:  &str = "/var/cache/hpm";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut parser    = lexopt::Parser::from_args(args);
    let mut command:  Option<String> = None;
    let mut sub_args: Vec<String>    = Vec::new();

    while let Some(arg) = parser.next().into_diagnostic()? {
        match arg {
            Short('h') | Long("help") => { print_help(); return Ok(()); }
            Short('V') | Long("version") => {
                println!("hpm {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Value(val) if command.is_none() => {
                command = Some(val.to_string_lossy().to_string());
            }
            Value(val) => {
                sub_args.push(val.to_string_lossy().to_string());
            }
            _ => {
                eprintln!("{} Unknown option: {:?}", "✗".red(), arg);
                print_help();
                return Ok(());
            }
        }
    }

    let command = command.unwrap_or_else(|| { print_help(); std::process::exit(0); });

    let result = match command.as_str() {
        // ── Package commands ────────────────────────────────────────────────
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

        // ── Query commands ──────────────────────────────────────────────────
        "search"     => commands::search::search(sub_args.first().cloned().unwrap_or_default()),
        "info"       => commands::info::info(sub_args.first().cloned().unwrap_or_default()),
        "list"       => commands::list::list_installed(),
        "outdated"   => commands::outdated::outdated(),
        "deps"       => commands::deps::deps(sub_args.first().cloned().unwrap_or_default()),
        "tags"       => cmd_tags(),
        "diff"       => commands::diff::diff(sub_args),

        // ── Maintenance commands ────────────────────────────────────────────
        "build"      => commands::build::build(sub_args.first().cloned().unwrap_or_default()),
        "clean"      => commands::clean::clean_cache(),
        "verify"     => {
            // hpm verify <pkg>  lub  hpm verify --import-key <keyid>
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
        "doctor"     => commands::doctor::doctor(),
        "repair"     => commands::repair::repair(),
        "lock"       => commands::lock::lock(sub_args),

        // ── Development commands ─────────────────────────────────────────────
        "create"     => commands::create::create(sub_args.first().cloned()),

        // ── Hidden: dev test suite (nie pokazywana w --help) ─────────────────
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
            eprintln!("{} {}", "✗".red(), e);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// hpm tags
// ---------------------------------------------------------------------------

fn cmd_tags() -> Result<()> {
    let repo_mgr = crate::repo::RepoManager::load_sync()?;
    let tags     = repo_mgr.all_tags();
    if tags.is_empty() {
        println!("{} No group tags defined in the repository.", "→".yellow());
        println!("  Tags are defined in repo.json and in individual package info.hk files.");
        return Ok(());
    }
    println!("{} Available group tags:\n", "→".blue());
    for tag in &tags {
        let pkgs  = repo_mgr.packages_for_tag(tag);
        let count = pkgs.len();
        let preview: Vec<&str> = pkgs.iter().take(5).map(|p| p.as_str()).collect();
        let suffix = if count > 5 { format!(" +{} more", count - 5) } else { String::new() };
        println!("  {} {:20} {} package(s): {}{}",
            "◆".cyan(),
            format!("@{}", tag).green(),
            count,
            preview.join(", ").dimmed(),
            suffix.dimmed()
        );
    }
    println!();
    println!("  Install a tag group : {}", "hpm install @<tag>".yellow());
    println!("  Search by tag       : {}", "hpm search @<tag>".yellow());
    Ok(())
}

// ---------------------------------------------------------------------------
// Help — wersja 0.8.0
// ---------------------------------------------------------------------------

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!("\n{} {}\n", "Hacker Package Manager (hpm)".bold().red(), version.red());
    println!("{}  hpm {} [options]\n", "Usage:".bold(), "<command>".yellow());

    println!("{}", "Package Commands:".bold().underline());
    println!("  {:<36} {}", "refresh".green(),                    "Update index and pre-fetch metadata");
    println!("  {:<36} {}", "install <pkg>[@<ver>]...".green(),   "Install packages");
    println!("  {:<36} {}", "install @<tag>".green(),             "Install all packages with group tag");
    println!("  {:<36} {}", "remove <pkg>[@<ver>]".green(),       "Remove package");
    println!("  {:<36} {}", "autoremove".green(),                 "Remove orphaned auto-installed packages");
    println!("  {:<36} {}", "update".green(),                     "Update all packages");
    println!("  {:<36} {}", "upgrade".green(),                    "Upgrade hpm itself");
    println!("  {:<36} {}", "switch <pkg> <ver>".green(),         "Switch active version");
    println!("  {:<36} {}", "rollback [<pkg>]".green(),           "Restore previous state or version");

    println!();
    println!("{}", "Query Commands:".bold().underline());
    println!("  {:<36} {}", "search <query|@tag>".green(),        "Search by name, tag or description");
    println!("  {:<36} {}", "info <package>".green(),             "Show package details and tags");
    println!("  {:<36} {}", "list".green(),                       "List installed packages");
    println!("  {:<36} {}", "outdated".green(),                   "Show packages with updates available");
    println!("  {:<36} {}", "deps <pkg>[@<ver>]".green(),         "Show dependency tree");
    println!("  {:<36} {}", "tags".green(),                       "List available group tags");
    println!("  {:<36} {}", "diff <pkg> <v1> [<v2>]".green(),    "Compare two package versions");

    println!();
    println!("{}", "Maintenance Commands:".bold().underline());
    println!("  {:<36} {}", "run <pkg> <bin> [args]".green(),     "Run binary (sandboxed)");
    println!("  {:<36} {}", "build [name]".green(),               "Package current directory (validates info.hk)");
    println!("  {:<36} {}", "clean".green(),                      "Remove cached repos and temp files");
    println!("  {:<36} {}", "verify <package>".green(),           "Verify SHA-256 + GPG signature");
    println!("  {:<36} {}", "verify --import-key <key>".green(),  "Import GPG key to trusted keyring");
    println!("  {:<36} {}", "pin <pkg> <ver>".green(),            "Pin a package version");
    println!("  {:<36} {}", "unpin <pkg>".green(),                "Unpin current version");
    println!("  {:<36} {}", "doctor".green(),                     "Diagnose store/state/wrapper consistency");
    println!("  {:<36} {}", "repair".green(),                     "Auto-fix issues found by doctor");
    println!("  {:<36} {}", "lock <subcmd>".green(),              "Manage hpm.lock for reproducible installs");

    println!();
    println!("{}", "Development Commands:".bold().underline());
    println!("  {:<36} {}", "create [<name>]".green(),            "Interactive package creation wizard");

    println!();
    println!("{}", "Options:".bold().underline());
    println!("  {}, {:<26} {}", "-h".yellow(), "--help".yellow(),    "Show this help");
    println!("  {}, {:<26} {}", "-V".yellow(), "--version".yellow(), "Show version");

    println!();
    println!("{}", "Group tags:".bold().underline());
    println!("  {}  →  install all packages tagged @development", "hpm install @development".yellow());
    println!("  {}  →  list all available tags", "hpm tags".yellow());
    println!("  {}  →  search within a tag group", "hpm search @cli".yellow());

    println!();
    println!("{}", "Lock file:".bold().underline());
    println!("  {}  →  generate hpm.lock", "hpm lock generate".yellow());
    println!("  {}  →  verify state matches lock (CI use)", "hpm lock check".yellow());

    println!();
    println!("{}", "Package repository format:".bold().underline());
    println!("  info.hk        Manifest (name, version, bins, deps, sandbox, tags)");
    println!("  build.toml     Build/download instructions (optional)");
    println!("  contents/      Pre-built files (optional when build.toml present)");
    println!("  hooks/         pre-install.sh, post-install.sh, pre-remove.sh, ...");
    println!("  info.hk.sig    GPG detached signature (optional, for verified packages)");
    println!();
}
