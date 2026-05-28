mod error;
mod manifest;
mod sandbox;
mod state;
mod repo;
mod commands;
mod utils;

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
        "build"      => commands::build::build(sub_args.first().cloned().unwrap_or_default()),
        "search"     => commands::search::search(sub_args.first().cloned().unwrap_or_default()),
        "info"       => commands::info::info(sub_args.first().cloned().unwrap_or_default()),
        "list"       => commands::list::list_installed(),
        "clean"      => commands::clean::clean_cache(),
        "pin"        => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm pin <package> <version>", "✗".red());
                std::process::exit(1);
            }
            commands::pin::pin(sub_args[0].clone(), sub_args[1].clone())
        }
        "unpin"      => commands::unpin::unpin(sub_args.first().cloned().unwrap_or_default()),
        "outdated"   => commands::outdated::outdated(),
        "verify"     => commands::verify::verify(sub_args.first().cloned().unwrap_or_default()),
        "deps"       => commands::deps::deps(sub_args.first().cloned().unwrap_or_default()),
        "autoremove" => commands::autoremove::autoremove(),
        "doctor"     => commands::doctor::doctor(),
        "repair"     => commands::repair::repair(),
        "rollback"   => commands::rollback::rollback(sub_args.first().cloned()),
        "create"     => commands::create::create(sub_args.first().cloned()),
        "tags"       => cmd_tags(),
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
        return Ok(());
    }
    println!("{} Available group tags:\n", "→".blue());
    for tag in &tags {
        let pkgs  = repo_mgr.packages_for_tag(tag);
        let count = pkgs.len();
        // FIXED: avoid temporary value dropped while borrowed
        let preview: Vec<&str> = pkgs.iter().take(5).map(|p| p.as_str()).collect();
        let suffix = if count > 5 {
            format!(" +{} more", count - 5)
        } else {
            String::new()
        };
        println!("  {} {:20} {} package(s): {}{}",
            "◆".cyan(),
            format!("@{}", tag).green(),
            count,
            preview.join(", "),
            suffix
        );
    }
    println!();
    println!("  Install a tag group: {}", "hpm install @<tag>".yellow());
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
    println!("  {:<36} {}", "refresh".green(),                    "Update index and pre-fetch metadata");
    println!("  {:<36} {}", "install <pkg>[@<ver>]...".green(),   "Install packages (atomic, resolves deps)");
    println!("  {:<36} {}", "install @<tag>".green(),             "Install all packages with group tag");
    println!("  {:<36} {}", "remove <pkg>[@<ver>]".green(),       "Remove package (warns on reverse deps)");
    println!("  {:<36} {}", "autoremove".green(),                 "Remove orphaned auto-installed packages");
    println!("  {:<36} {}", "update".green(),                     "Update all packages (incremental fetch)");
    println!("  {:<36} {}", "upgrade".green(),                    "Upgrade hpm itself");
    println!("  {:<36} {}", "switch <pkg> <ver>".green(),         "Switch active version of a package");
    println!("  {:<36} {}", "rollback [<pkg>]".green(),           "Restore previous state or package version");

    println!();
    println!("{}", "Query Commands:".bold().underline());
    println!("  {:<36} {}", "search <query|@tag>".green(),        "Search by name/tag/description");
    println!("  {:<36} {}", "info <package>".green(),             "Show package details");
    println!("  {:<36} {}", "list".green(),                       "List all installed packages");
    println!("  {:<36} {}", "outdated".green(),                   "Show packages with updates available");
    println!("  {:<36} {}", "deps <pkg>[@<ver>]".green(),         "Show full dependency tree");
    println!("  {:<36} {}", "tags".green(),                       "List available group tags");

    println!();
    println!("{}", "Maintenance Commands:".bold().underline());
    println!("  {:<36} {}", "run <pkg> <bin> [args]".green(),     "Run a binary (sandboxed)");
    println!("  {:<36} {}", "build <name>".green(),               "Package current directory");
    println!("  {:<36} {}", "clean".green(),                      "Remove cached git repos + temp files");
    println!("  {:<36} {}", "verify <package>".green(),           "Verify package integrity (SHA-256)");
    println!("  {:<36} {}", "pin <pkg> <ver>".green(),            "Pin a package version");
    println!("  {:<36} {}", "unpin <pkg>".green(),                "Unpin current version");
    println!("  {:<36} {}", "doctor".green(),                     "Diagnose store/state/wrapper consistency");
    println!("  {:<36} {}", "repair".green(),                     "Auto-fix issues found by doctor");

    println!();
    println!("{}", "Development Commands:".bold().underline());
    println!("  {:<36} {}", "create [<name>]".green(),            "Interactive package creation wizard");

    println!();
    println!("{}", "Options:".bold().underline());
    println!("  {}, {:<26} {}", "-h".yellow(), "--help".yellow(),    "Show this help");
    println!("  {}, {:<26} {}", "-V".yellow(), "--version".yellow(), "Show version");

    println!();
    println!("{}", "Group tags:".bold().underline());
    println!("  {}", "hpm install @development".yellow());
    println!("  {}", "hpm install @cli @tools".yellow());
    println!("  {}", "hpm search @development".yellow());
    println!("  {}", "hpm tags".yellow());

    println!();
    println!("{}", "Package repository format:".bold().underline());
    println!("  info.hk      Manifest (name, version, deps, sandbox, desktop, tags)");
    println!("  build.toml   Build/download instructions (optional)");
    println!("  contents/    Pre-built files (optional when build.toml present)");
    println!();
}
