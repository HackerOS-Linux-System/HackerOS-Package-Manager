mod error;
mod manifest;
mod sandbox;
mod state;
mod repo;
mod commands;
mod utils;

use clap::{Parser, Subcommand};
use anyhow::Result;
use colored::Colorize;
use tokio::runtime::Runtime;
use std::process::exit;

pub const STORE_PATH: &str = "/usr/lib/HackerOS/hpm/store/";

#[derive(Parser)]
#[command(author, version, about = "Hacker Package Manager", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Refresh,
    Install { specs: Vec<String> },
    Remove { spec: String },
    Update,
    Switch { package: String, version: String },
    Upgrade,
    Run {
        package_spec: String,
        bin: String,
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    Build { name: String },
    Search { query: String },
    Info { package: String },
    List,
    Clean,
    Pin { package: String, version: String },
    Unpin { package: String },
    Outdated,
    Verify { package: String },
    Deps { spec: String },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Refresh => refresh(),
        Commands::Install { specs } => commands::install::install(specs),
        Commands::Remove { spec } => commands::remove::remove(spec),
        Commands::Update => commands::update::update(),
        Commands::Switch { package, version } => commands::switch_version(package, version),
        Commands::Upgrade => commands::upgrade::upgrade(),
        Commands::Run { package_spec, bin, args } => commands::run::run(package_spec, bin, args),
        Commands::Build { name } => commands::build::build(name),
        Commands::Search { query } => commands::search::search(query),
        Commands::Info { package } => commands::info::info(package),
        Commands::List => commands::list::list_installed(),
        Commands::Clean => commands::clean::clean_cache(),
        Commands::Pin { package, version } => commands::pin::pin(package, version),
        Commands::Unpin { package } => commands::unpin::unpin(package),
        Commands::Outdated => commands::outdated::outdated(),
        Commands::Verify { package } => commands::verify::verify(package),
        Commands::Deps { spec } => commands::deps::deps(spec),
    };

    if let Err(e) = result {
        eprintln!("{}", e);
        exit(1);
    }
}

fn refresh() -> Result<()> {
    let rt = Runtime::new()?;
    let repo_mgr = rt.block_on(repo::RepoManager::load())?;
    rt.block_on(repo_mgr.refresh())?;
    println!("{} Package index refreshed.", "✔".green());
    Ok(())
}
