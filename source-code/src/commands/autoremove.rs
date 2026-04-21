use miette::{Result, IntoDiagnostic};
use colored::Colorize;
use std::io::Write;
use crate::{
    state::State,
    utils::{acquire_lock, release_lock},
};
use super::remove::remove_version;

pub fn autoremove() -> Result<()> {
    let lock = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let mut state = State::load()?;

    // Find orphans iteratively — removing one orphan may expose another
    let mut removed_total = 0usize;
    loop {
        let orphans = state.orphans();
        if orphans.is_empty() { break; }

        println!("{} The following packages were installed as dependencies and are no longer needed:\n",
                 "→".yellow());
        for (name, ver) in &orphans {
            println!("  {} {}@{}", "–".red(), name.cyan(), ver);
        }
        println!();

        eprint!("Remove {} package(s)? [y/N] ", orphans.len());
        std::io::stderr().flush().into_diagnostic()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).into_diagnostic()?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("{} Aborted.", "→".yellow());
            return Ok(());
        }

        state.push_snapshot(&format!("pre-autoremove {} packages", orphans.len()));

        for (name, ver) in &orphans {
            remove_version(name, ver, &mut state)?;
            println!("  {} Removed {}@{}", "✔".green(), name.cyan(), ver);
            removed_total += 1;
        }
        // Loop again — previous orphans' deps may now also be orphans
    }

    if removed_total == 0 {
        println!("{} Nothing to remove — no orphaned packages found.", "✔".green());
    } else {
        state.save()?;
        println!("\n{} Removed {} orphaned package(s).", "✔".green(), removed_total);
    }

    Ok(())
}
