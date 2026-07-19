use miette::{Result, IntoDiagnostic, bail};
use colored::Colorize;
use std::fs;
use crate::utils::{download_file, compare_versions};

const VERSION_URL:   &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/Hacker-Package-Manager/main/version.hacker";
const RELEASES_BASE: &str = "https://github.com/HackerOS-Linux-System/Hacker-Package-Manager/releases/download/v";

/// Gdzie trzymamy metadane wersji hpm samego siebie. To tylko jeden mały
/// plik JSON — nie ma powodu żeby wymagał roota nawet gdy `hpm` binarnie
/// mieszka w /usr/bin (system-wide install), więc zawsze idzie do naszego
/// własnego katalogu użytkownika.
fn local_version_file() -> String { format!("{}/hpm-version.json", crate::db_dir()) }

/// Towarzyszący plik "backend" (dawniej zawsze /usr/lib/HackerOS/hpm/backend,
/// niezależnie od tego gdzie żył sam `hpm`). Trzymamy go obok metadanych
/// wersji, w naszym katalogu użytkownika — nie ma potrzeby, żeby wymagał
/// roota, to nie jest sam binarny `hpm`.
fn local_backend_file() -> String { format!("{}/backend", crate::db_dir()) }

/// `hpm upgrade` aktualizuje BINARKĘ HPM SAMĄ W SOBIE (nie pakiety, którymi
/// zarządza — do tego służy `hpm update`). W przeciwieństwie do reszty
/// komend, to jedyne miejsce gdzie root bywa faktycznie potrzebny — ale
/// TYLKO jeśli `hpm` faktycznie mieszka w katalogu systemowym (np. /usr/bin,
/// bo tak zainstalowała go dystrybucja). Jeśli ktoś ma `hpm` w swoim własnym
/// `~/.local/bin` (np. ręcznie pobrany, albo zbudowany z sourców), upgrade
/// dzieje się całkowicie bez roota — sprawdzamy to empirycznie (próbujemy
/// zapisu), zamiast z góry zakładać że sudo jest potrzebne.
pub fn upgrade() -> Result<()> {
    let lock   = crate::utils::acquire_lock()?;
    let _guard = scopeguard::guard(lock, |_| crate::utils::release_lock());

    let tmp_version = "/tmp/hpm-version.hacker";
    download_file(VERSION_URL, tmp_version)?;
    let remote_version = fs::read_to_string(tmp_version).into_diagnostic()?.trim().to_string();

    let version_path = local_version_file();
    let local_version = if fs::metadata(&version_path).is_ok() {
        let data: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&version_path).into_diagnostic()?
        ).into_diagnostic()?;
        data["version"].as_str().unwrap_or("0.0").to_string()
    } else {
        "0.0".to_string()
    };

    if compare_versions(&remote_version, &local_version) != std::cmp::Ordering::Greater {
        println!("{} HPM is already up to date ({})", "✔".red(), local_version.white());
        return Ok(());
    }

    println!("{} Upgrading HPM from {} to {}...",
             "→".bright_black(), local_version.white(), remote_version.white());

    let current_exe = std::env::current_exe().into_diagnostic()?;
    let hpm_url      = format!("{}{}/hpm",     RELEASES_BASE, remote_version);
    let backend_url  = format!("{}{}/backend", RELEASES_BASE, remote_version);

    // Pobierz do plików tymczasowych najpierw — jeśli sieć/serwer zawiedzie,
    // nigdy nie dotykamy działającej binarki hpm.
    let tmp_hpm     = "/tmp/hpm-upgrade-new";
    let tmp_backend = "/tmp/hpm-upgrade-backend-new";
    download_file(&hpm_url, tmp_hpm)?;
    download_file(&backend_url, tmp_backend)?;
    crate::utils::make_executable(std::path::Path::new(tmp_hpm))?;
    crate::utils::make_executable(std::path::Path::new(tmp_backend))?;

    match replace_binary_in_place(&current_exe, tmp_hpm) {
        Ok(()) => {
            let _ = fs::rename(tmp_backend, local_backend_file());
        }
        Err(e) => {
            let _ = fs::remove_file(tmp_hpm);
            let _ = fs::remove_file(tmp_backend);
            eprintln!("  {} Could not replace {} ({})", "✗".red(), current_exe.display(), e);
            if is_system_path(&current_exe) {
                eprintln!("  {} {} lives in a system directory — retry with:",
                          "→".bright_black(), current_exe.display());
                eprintln!("      {}", "sudo hpm upgrade".white().bold());
            } else {
                eprintln!("  {} That's your own directory, not a system path — check its permissions:",
                          "→".bright_black());
                eprintln!("      {}", format!("ls -la {}", current_exe.display()).white());
            }
            bail!("Upgrade aborted, current hpm binary left untouched");
        }
    }

    let new_state = serde_json::json!({ "version": remote_version });
    fs::write(&version_path, new_state.to_string()).into_diagnostic()?;
    println!("{} Upgrade complete to version {}", "✔".red(), remote_version.red());
    Ok(())
}

/// True dla katalogów, które NORMALNIE wymagają roota do zapisu na typowym
/// Linuksie — używane tylko do lepszego komunikatu błędu, nie do decyzji czy
/// w ogóle próbować (zawsze próbujemy najpierw, wynik mówi prawdę).
fn is_system_path(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("/usr/") || s.starts_with("/bin/") || s.starts_with("/sbin/") || s.starts_with("/opt/")
}

/// Podmienia binarkę pod `target` na zawartość `new_binary`, w miejscu, przez
/// zapis-do-tmp-i-rename (atomowo — proces `hpm` mógłby akurat być
/// uruchomiony z drugiego wywołania w tym samym momencie). Jeśli katalog
/// docelowy nie jest zapisywalny przez bieżącego użytkownika, zwraca błąd
/// zamiast automatycznie doskakiwać do `sudo` — o tym decyduje wywołujący.
fn replace_binary_in_place(target: &std::path::Path, new_binary: &str) -> Result<()> {
    let dir = target.parent().unwrap_or_else(|| std::path::Path::new("/"));
    let tmp_in_place = dir.join(".hpm-upgrade.tmp");

    fs::copy(new_binary, &tmp_in_place).into_diagnostic()?;
    crate::utils::make_executable(&tmp_in_place)?;
    fs::rename(&tmp_in_place, target).into_diagnostic()?;
    Ok(())
}
