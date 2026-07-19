use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::Path;
use crate::{
    state::State,
    utils::{acquire_lock, release_lock},
};

pub fn remove(spec: String) -> Result<()> {
    let lock   = acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| release_lock());

    let mut state = State::load()?;

    let (pkg_name, version) = if spec.contains('@') {
        let mut parts = spec.splitn(2, '@');
        (parts.next().unwrap().to_string(), Some(parts.next().unwrap().to_string()))
    } else {
        (spec.clone(), None)
    };

    if !state.packages.contains_key(&pkg_name) {
        bail!("Package '{}' is not installed", pkg_name);
    }

    // Sprawdź odwrotne zależności
    let rdeps = state.reverse_deps(&pkg_name);
    if !rdeps.is_empty() {
        let remaining_rdeps: Vec<&String> = if let Some(ref ver) = version {
            rdeps.iter()
                .filter(|_dep| {
                    let other_versions_exist = state.packages.get(&pkg_name)
                        .map(|vs| vs.len() > 1)
                        .unwrap_or(false);
                    !other_versions_exist
                })
                .collect()
        } else {
            rdeps.iter().collect()
        };

        if !remaining_rdeps.is_empty() {
            eprintln!("{} The following packages depend on {}:", "⚠".bright_black(), pkg_name.white());
            for dep in &remaining_rdeps {
                eprintln!("  {} {}", "→".bright_black(), dep);
            }
            eprint!("Remove anyway? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).into_diagnostic()?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("{} Aborted.", "→".bright_black());
                return Ok(());
            }
        }
    }

    state.push_snapshot(&format!("pre-remove {}", spec));

    if let Some(ver) = version {
        remove_version(&pkg_name, &ver, &mut state)?;
        println!("{} {}@{} removed", "✔".red(), pkg_name.white(), ver.white());
    } else {
        let versions: Vec<String> = state.packages.get(&pkg_name)
            .unwrap().keys().cloned().collect();
        for ver in &versions {
            remove_version(&pkg_name, ver, &mut state)?;
        }
        println!("{} {} removed", "✔".red(), pkg_name.white());
    }

    state.save()?;
    Ok(())
}

pub fn remove_version(pkg_name: &str, version: &str, state: &mut State) -> Result<()> {
    let pkg_path = Path::new(crate::store_path()).join(pkg_name).join(version);
    if !pkg_path.exists() {
        bail!("Path {} does not exist", pkg_path.display());
    }
    crate::squash::ensure_mounted(&pkg_path)?;

    let mut staged_post_remove_hooks: Option<std::path::PathBuf> = None;

    // BUG NAPRAWIONY (znaleziony przez realny test `hpm rollback`): wrapper i
    // integracja desktopowa są WSPÓLNE dla całego pakietu (wskazują na
    // `current`, nie na konkretną wersję — patrz `which_hpm_for_wrappers` i
    // wrapper content: `hpm run <pkg> <bin>` bez numeru wersji). Poprzednio
    // usuwaliśmy je bezwarunkowo przy USUNIĘCIU JAKIEJKOLWIEK wersji, nawet
    // gdy inna wersja tego samego pakietu zostawała zainstalowana i aktywna —
    // efekt: `hpm rollback` (usuwający starą wersję z historii) kasował
    // działający wrapper aktualnie zainstalowanej wersji. Usuwamy
    // wrapper/desktop TYLKO gdy to naprawdę OSTATNIA wersja tego pakietu.
    let is_last_version = state.packages.get(pkg_name)
        .map(|vers| vers.len() <= 1 || (vers.len() == 1 && vers.contains_key(version)))
        .unwrap_or(true);

    // Usuń wrappery /usr/bin — sprawdzaj też alternatywne nazwy z WrapperNames
    if let Ok(manifest) = crate::manifest::Manifest::load_from_path(pkg_path.to_str().unwrap()) {
        let ctx = crate::hooks::HookContext {
            pkg_name: pkg_name, pkg_version: version,
            store_path: crate::store_path(), old_version: None,
        };

        // Pre-remove: katalog pakietu jeszcze istnieje, hook może np. wyrejestrować usługę.
        if crate::hooks::hook_exists(&pkg_path, crate::hooks::HookKind::PreRemove) {
            crate::hooks::run_hook(&pkg_path, crate::hooks::HookKind::PreRemove, &ctx, &manifest)?;
        }

        // Post-remove musi zadziałać PO fizycznym usunięciu plików pakietu — ale
        // hook mieszka właśnie w tych plikach (`hooks/post-remove.*`). Skopiuj
        // sam katalog hooks/ do tymczasowej lokalizacji PRZED usunięciem, żeby
        // dało się go uruchomić już po `fs::remove_dir_all`.
        staged_post_remove_hooks = {
            let hooks_src = pkg_path.join("hooks");
            if hooks_src.exists() && crate::hooks::hook_exists(&pkg_path, crate::hooks::HookKind::PostRemove) {
                let staging = std::env::temp_dir()
                    .join(format!("hpm-post-remove-{}-{}-{}", pkg_name, version, std::process::id()));
                let staged_hooks = staging.join("hooks");
                if fs::create_dir_all(&staged_hooks).is_ok() {
                    let _ = crate::utils::copy_dir_recursive(&hooks_src, &staged_hooks);
                    Some(staging)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let wn = crate::state::WrapperNames::load();
        if is_last_version {
            for bin in &manifest.bins {
                // Sprawdź zapamiętaną niestandardową nazwę
                let wrapper_name = wn.get(pkg_name, bin)
                    .unwrap_or(bin.as_str())
                    .to_string();
                let wrapper = Path::new(crate::bin_dir()).join(&wrapper_name);
                if wrapper.exists() {
                    // Upewnij się że to nasz wrapper, nie cudzy
                    let content = fs::read_to_string(&wrapper).unwrap_or_default();
                    if content.contains(&format!("hpm run {} ", pkg_name)) {
                        fs::remove_file(&wrapper).into_diagnostic()?;
                    }
                }
                // Też sprawdź domyślną nazwę (na wypadek gdyby wrapper-names.json nie był aktualny)
                if wrapper_name != *bin {
                    let default_wrapper = Path::new(crate::bin_dir()).join(bin);
                    if default_wrapper.exists() {
                        let content = fs::read_to_string(&default_wrapper).unwrap_or_default();
                        if content.contains(&format!("hpm run {} ", pkg_name)) {
                            fs::remove_file(&default_wrapper).into_diagnostic()?;
                        }
                    }
                }
            }

            // Desktop integration dla GUI
            if manifest.is_gui || manifest.sandbox.gui || manifest.sandbox.full_gui {
                remove_desktop_integration(pkg_name)?;
            }
        }
    }

    crate::squash::unmount(&pkg_path)?;
    fs::remove_dir_all(&pkg_path).into_diagnostic()?;
    crate::squash::remove_sidecar(&pkg_path)?;
    state.remove_package_version(pkg_name, version);

    if let Some(staging) = staged_post_remove_hooks {
        let ctx = crate::hooks::HookContext {
            pkg_name: pkg_name, pkg_version: version,
            store_path: crate::store_path(), old_version: None,
        };
        let manifest_for_hook = crate::manifest::Manifest::default();
        let _ = crate::hooks::run_hook(&staging, crate::hooks::HookKind::PostRemove, &ctx, &manifest_for_hook);
        let _ = fs::remove_dir_all(&staging);
    }

    // Zaktualizuj symlink current
    let current_link = Path::new(crate::store_path()).join(pkg_name).join("current");
    if let Ok(target) = fs::read_link(&current_link) {
        if target == Path::new(version) {
            fs::remove_file(&current_link).into_diagnostic()?;
            if let Some(vers) = state.packages.get(pkg_name) {
                let mut remaining: Vec<&String> = vers.keys().collect();
                remaining.sort_by(|a, b| crate::utils::compare_versions(a, b));
                if let Some(newest) = remaining.last() {
                    std::os::unix::fs::symlink(newest, &current_link).into_diagnostic()?;
                    println!("  {} Switched current to {}", "→".bright_black(), newest.white());
                }
            }
        }
    }

    Ok(())
}

fn remove_desktop_integration(pkg_name: &str) -> Result<()> {
    let desktop = Path::new(crate::desktop_dir()).join(format!("{}.desktop", pkg_name));
    if desktop.exists() { fs::remove_file(&desktop).into_diagnostic()?; }

    for size in &["16x16","32x32","48x48","64x64","128x128","256x256","scalable"] {
        for ext in &["png","svg","xpm"] {
            let icon = Path::new(crate::icon_dir())
                .join(format!("{}/apps", size))
                .join(format!("{}.{}", pkg_name, ext));
            if icon.exists() { let _ = fs::remove_file(&icon); }
        }
    }
    for ext in &["png","svg","xpm"] {
        let pixmap = Path::new(crate::pixmap_dir()).join(format!("{}.{}", pkg_name, ext));
        if pixmap.exists() { let _ = fs::remove_file(&pixmap); }
    }

    let _ = std::process::Command::new("update-desktop-database")
        .arg(crate::desktop_dir()).status();
    let _ = std::process::Command::new("gtk-update-icon-cache")
        .args(["-f","-t",crate::icon_dir()]).status();

    Ok(())
}
