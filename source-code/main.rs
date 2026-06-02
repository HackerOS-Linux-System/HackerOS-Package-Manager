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
        "build"      => commands::build::build(sub_args.first().cloned().unwrap_or_default()),
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

    // Operacje które zawsze wymagają sudo
    let needs_root = matches!(command,
        "install" | "remove" | "update" | "upgrade" | "rollback" |
        "autoremove" | "repair" | "switch" | "refresh"
    );

    if needs_root {
        println!("  {} This command writes to system directories and requires root privileges.",
                 "→".yellow());
        println!();
        println!("  Please run with sudo:");
        println!();
        // Zrekonstruuj oryginalne wywołanie z args
        let original_args: Vec<String> = std::env::args().skip(1).collect();
        println!("    {}", format!("sudo hpm {}", original_args.join(" ")).bold().cyan());
        println!();
        println!("  {} hpm installs packages to {} which requires root.",
                 "ℹ".blue(), "/usr/lib/HackerOS/hpm/store/".dimmed());
        println!("  {} Wrappers are created in {} which also requires root.",
                 "ℹ".blue(), "/usr/bin/".dimmed());
    } else {
        println!("  {} Operation failed with permission error.", "→".yellow());
        println!();
        println!("  Try running with sudo:");
        let original_args: Vec<String> = std::env::args().skip(1).collect();
        println!("    {}", format!("sudo hpm {}", original_args.join(" ")).bold().cyan());
    }
    println!();
}

// ---------------------------------------------------------------------------
// hpm tags
// ---------------------------------------------------------------------------

fn cmd_tags() -> Result<()> {
    let repo_mgr = crate::repo::RepoManager::load_sync()?;
    let tags     = repo_mgr.all_tags();
    if tags.is_empty() {
        println!("{} No group tags found.", "→".yellow());
        println!("  Tags are defined in each package's {} file.", "info.hk".yellow());
        println!("  Run {} to fetch metadata from all packages.", "hpm refresh".yellow());
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
// Help
// ---------------------------------------------------------------------------

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!("\n{} {}\n", "Hacker Package Manager (hpm)".bold().red(), version.red());
    println!("{}  hpm {} [options]\n", "Usage:".bold(), "<command>".yellow());

    println!("{}", "Package Commands:".bold().underline());
    println!("  {:<38} {}", "refresh".green(),                    "Update index and pre-fetch metadata");
    println!("  {:<38} {}", "install <pkg>[@<ver>]...".green(),   "Install packages (requires sudo)");
    println!("  {:<38} {}", "install @<tag>".green(),             "Install all packages with group tag");
    println!("  {:<38} {}", "remove <pkg>[@<ver>]".green(),       "Remove package (requires sudo)");
    println!("  {:<38} {}", "autoremove".green(),                 "Remove orphaned packages");
    println!("  {:<38} {}", "update".green(),                     "Update all packages");
    println!("  {:<38} {}", "upgrade".green(),                    "Upgrade hpm itself");
    println!("  {:<38} {}", "switch <pkg> <ver>".green(),         "Switch active version");
    println!("  {:<38} {}", "rollback [<pkg>]".green(),           "Restore previous state");

    println!();
    println!("{}", "Query Commands:".bold().underline());
    println!("  {:<38} {}", "search <query|@tag>".green(),        "Search packages");
    println!("  {:<38} {}", "info <package>".green(),             "Show package details");
    println!("  {:<38} {}", "list".green(),                       "List installed packages");
    println!("  {:<38} {}", "outdated".green(),                   "Show packages with updates");
    println!("  {:<38} {}", "deps <pkg>[@<ver>]".green(),         "Show dependency tree");
    println!("  {:<38} {}", "tags".green(),                       "List available group tags");
    println!("  {:<38} {}", "diff <pkg> <v1> [<v2>]".green(),    "Compare two package versions");

    println!();
    println!("{}", "Maintenance Commands:".bold().underline());
    println!("  {:<38} {}", "run <pkg> <bin> [args]".green(),     "Run binary (sandboxed)");
    println!("  {:<38} {}", "build [name]".green(),               "Package current directory");
    println!("  {:<38} {}", "clean".green(),                      "Remove cached repos + temp files");
    println!("  {:<38} {}", "clean --all".green(),                "Also remove old store versions");
    println!("  {:<38} {}", "verify <package>".green(),           "Verify SHA-256 + GPG signature");
    println!("  {:<38} {}", "verify --import-key <key>".green(),  "Import GPG key to trusted keyring");
    println!("  {:<38} {}", "pin <pkg> <ver>".green(),            "Pin a package version");
    println!("  {:<38} {}", "unpin <pkg>".green(),                "Unpin current version");
    println!("  {:<38} {}", "doctor".green(),                     "Diagnose consistency issues");
    println!("  {:<38} {}", "repair".green(),                     "Auto-fix issues found by doctor");
    println!("  {:<38} {}", "lock <subcmd>".green(),              "Manage hpm.lock (reproducible installs)");

    println!();
    println!("{}", "Development Commands:".bold().underline());
    println!("  {:<38} {}", "create [<name>]".green(),            "Interactive package creation wizard");

    println!();
    println!("{}", "Options:".bold().underline());
    println!("  {}, {:<28} {}", "-h".yellow(), "--help".yellow(),    "Show this help");
    println!("  {}, {:<28} {}", "-V".yellow(), "--version".yellow(), "Show version");

    println!();
    println!("{}", "Group tags:".bold().underline());
    println!("  {}  →  install all @development packages", "hpm install @development".yellow());
    println!("  {}  →  list all available tags",           "hpm tags".yellow());

    println!();
    println!("{}", "Hooks (Hacker Lang):".bold().underline());
    println!("  Package hooks are placed in {} subdirectory:", "hooks/".cyan());
    println!("  {:<30} {}", "pre-install.hl".cyan(),  "runs before install (blocks on failure)");
    println!("  {:<30} {}", "post-install.hl".cyan(), "runs after install");
    println!("  {:<30} {}", "pre-remove.hl".cyan(),   "runs before removal (blocks on failure)");
    println!("  {:<30} {}", "post-remove.hl".cyan(),  "runs after removal");
    println!("  {:<30} {}", "post-update.hl".cyan(),  "runs after update");
    println!("  Also supported: {} {} {}", ".py".dimmed(), ".rb".dimmed(), ".sh".dimmed());

    println!();
    println!("{}", "Package format:".bold().underline());
    println!("  info.hk          Manifest (name, version, bins, deps, sandbox, tags)");
    println!("  build.toml       Build/download instructions (optional)");
    println!("  contents/        Pre-built files");
    println!("  hooks/           Hacker Lang hooks (.hl) — pre/post install/remove");
    println!("  info.hk.sig      GPG signature (optional)");
    println!();
    println!("  {} Most package operations require {}", "ℹ".blue(), "sudo hpm <command>".yellow());
    println!();
}
