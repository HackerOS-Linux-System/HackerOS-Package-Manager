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
pub const CACHE_DIR: &str = "/var/cache/hpm";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut parser = lexopt::Parser::from_args(args);
    let mut command: Option<String> = None;
    let mut sub_args: Vec<String> = Vec::new();

    while let Some(arg) = parser.next().into_diagnostic()? {
        match arg {
            Short('h') | Long("help") => {
                print_help();
                return Ok(());
            }
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

    let command = command.unwrap_or_else(|| {
        print_help();
        std::process::exit(0);
    });

    let result = match command.as_str() {
        "refresh"  => commands::refresh::refresh(),
        "install"  => commands::install::install(sub_args),
        "remove"   => commands::remove::remove(sub_args.first().cloned().unwrap_or_default()),
        "update"   => commands::update::update(),
        "switch"   => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm switch <package> <version>", "✗".red());
                std::process::exit(1);
            }
            commands::switch_version(sub_args[0].clone(), sub_args[1].clone())
        }
        "upgrade"  => commands::upgrade::upgrade(),
        "run"      => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm run <package> <bin> [args...]", "✗".red());
                std::process::exit(1);
            }
            let package = sub_args[0].clone();
            let bin     = sub_args[1].clone();
            let args    = sub_args[2..].to_vec();
            commands::run::run(package, bin, args)
        }
        "build"    => commands::build::build(sub_args.first().cloned().unwrap_or_default()),
        "search"   => commands::search::search(sub_args.first().cloned().unwrap_or_default()),
        "info"     => commands::info::info(sub_args.first().cloned().unwrap_or_default()),
        "list"     => commands::list::list_installed(),
        "clean"    => commands::clean::clean_cache(),
        "pin"      => {
            if sub_args.len() < 2 {
                eprintln!("{} Usage: hpm pin <package> <version>", "✗".red());
                std::process::exit(1);
            }
            commands::pin::pin(sub_args[0].clone(), sub_args[1].clone())
        }
        "unpin"    => commands::unpin::unpin(sub_args.first().cloned().unwrap_or_default()),
        "outdated" => commands::outdated::outdated(),
        "verify"   => commands::verify::verify(sub_args.first().cloned().unwrap_or_default()),
        "deps"     => commands::deps::deps(sub_args.first().cloned().unwrap_or_default()),
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

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    println!("\n{} {}\n", "Hacker Package Manager (hpm)".bold().cyan(), version.cyan());
    println!("{}  hpm {} [options]\n", "Usage:".bold(), "<command>".yellow());

    println!("{}", "Package Commands:".bold().underline());
    println!("  {:<30} {}", "refresh".green(),
             "Update package index and pre-fetch metadata");
    println!("  {:<30} {}", "install <pkg>[@<ver>]...".green(),
             "Install packages (resolves deps automatically)");
    println!("  {:<30} {}", "remove <pkg>[@<ver>]".green(),
             "Remove a package or a specific version");
    println!("  {:<30} {}", "update".green(),
             "Update all non-pinned packages to latest");
    println!("  {:<30} {}", "upgrade".green(),
             "Upgrade hpm itself");
    println!("  {:<30} {}", "switch <pkg> <ver>".green(),
             "Switch active version of a package");

    println!();
    println!("{}", "Query Commands:".bold().underline());
    println!("  {:<30} {}", "search <query>".green(),
             "Search by name/description (fast, no git clone)");
    println!("  {:<30} {}", "info <package>".green(),
             "Show package details and build info");
    println!("  {:<30} {}", "list".green(),
             "List all installed packages");
    println!("  {:<30} {}", "outdated".green(),
             "Show packages with available updates");
    println!("  {:<30} {}", "deps <pkg>[@<ver>]".green(),
             "Show full dependency tree");

    println!();
    println!("{}", "Maintenance Commands:".bold().underline());
    println!("  {:<30} {}", "run <pkg> <bin> [args]".green(),
             "Run a binary from an installed package (sandboxed)");
    println!("  {:<30} {}", "build <name>".green(),
             "Package the current directory as a hpm package");
    println!("  {:<30} {}", "clean".green(),
             "Remove cached git repos and temp files");
    println!("  {:<30} {}", "verify <package>".green(),
             "Verify package integrity via checksum");
    println!("  {:<30} {}", "pin <pkg> <ver>".green(),
             "Pin a package to a specific version");
    println!("  {:<30} {}", "unpin <pkg>".green(),
             "Unpin the current version");

    println!();
    println!("{}", "Options:".bold().underline());
    println!("  {}, {:<24} {}", "-h".yellow(), "--help".yellow(),    "Show this help");
    println!("  {}, {:<24} {}", "-V".yellow(), "--version".yellow(), "Show version");
    println!();
    println!("{}", "Package repository format:".bold().underline());
    println!("  info.hk      Manifest (name, version, description, deps, sandbox)");
    println!("  build.toml   Build/download instructions (optional)");
    println!("  contents/    Pre-built files (optional, used when build.toml absent)");
    println!();
}
