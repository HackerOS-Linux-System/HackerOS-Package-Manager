use miette::{Result, bail, IntoDiagnostic};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;

pub const LOCK_PATH: &str = "/tmp/hpm.lock";

pub fn acquire_lock() -> Result<fs::File> {
    use fs2::FileExt;
    let file = fs::File::create(LOCK_PATH).into_diagnostic()?;
    // BUG NAPRAWIONY (znaleziony przez realny test dwóch nakładających się
    // `hpm install`): blokada poprawnie zapobiega współbieżnym operacjom
    // (bezpieczeństwo działa), ale błąd był surowym "Resource temporarily
    // unavailable (os error 11)" — nic nie mówiącym użytkownikowi, co się
    // stało. Teraz jasno tłumaczymy, że to inny hpm trzyma blokadę.
    file.try_lock_exclusive().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            miette::miette!(
                "Another hpm process is already running (install/remove/update/...).\n  \
  Wait for it to finish, or if you're sure nothing is running, remove the stale lock:\n  \
  {}",
                LOCK_PATH
            )
        } else {
            miette::miette!("Failed to acquire lock at {}: {}", LOCK_PATH, e)
        }
    })?;
    Ok(file)
}

pub fn release_lock() {
    let _ = fs::remove_file(LOCK_PATH);
}

pub fn compute_dir_hash(dir: &Path) -> Result<String> {
    let entries: Vec<_> = WalkDir::new(dir)
    .sort_by(|a, b| a.file_name().cmp(b.file_name()))
    .into_iter()
    .filter_map(|e| e.ok())
    .filter(|e| e.file_type().is_file())
    .map(|e| e.path().to_owned())
    .collect();
    let mut hasher = Sha256::new();
    for file_path in entries {
        let data = fs::read(&file_path).into_diagnostic()?;
        hasher.update(&data);
    }
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// SHA-256 pojedynczego pliku (np. archiwum .hpm) — używane przez
/// `hpm build` (suma kontrolna obok archiwum) i `hpm install --release`
/// (weryfikacja integralności pobranego .hpm przed rozpakowaniem).
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let data = fs::read(path).into_diagnostic()?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).into_diagnostic()?;
    for entry in fs::read_dir(src).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let ty = entry.file_type().into_diagnostic()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).into_diagnostic()?;
        }
    }
    Ok(())
}

pub fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).into_diagnostic()?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).into_diagnostic()?;
    Ok(())
}

pub fn run_command(args: &[String]) -> Result<i32> {
    let status = Command::new(&args[0]).args(&args[1..]).status().into_diagnostic()?;
    Ok(status.code().unwrap_or(1))
}

pub fn download_file(url: &str, dest: &str) -> Result<()> {
    let args = vec![
        "curl".to_string(),
        "-L".to_string(),
        "--progress-bar".to_string(),
        "-o".to_string(),
        dest.to_string(),
        url.to_string(),
    ];
    let code = run_command(&args)?;
    if code != 0 {
        bail!("Download failed with code {}", code);
    }
    Ok(())
}

pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts_a: Vec<&str> = a.split(|c| c == '.' || c == '-').collect();
    let parts_b: Vec<&str> = b.split(|c| c == '.' || c == '-').collect();
    for i in 0..parts_a.len().max(parts_b.len()) {
        let part_a = parts_a.get(i).unwrap_or(&"0");
        let part_b = parts_b.get(i).unwrap_or(&"0");
        if part_a.parse::<u32>().is_ok() && part_b.parse::<u32>().is_ok() {
            let num_a = part_a.parse::<u32>().unwrap();
            let num_b = part_b.parse::<u32>().unwrap();
            if num_a != num_b {
                return num_a.cmp(&num_b);
            }
        } else {
            if part_a != part_b {
                return part_a.cmp(part_b);
            }
        }
    }
    std::cmp::Ordering::Equal
}

pub fn satisfies(ver: &str, req: &str) -> bool {
    if req.is_empty() {
        return true;
    }
    if req.starts_with(">=") {
        let req_ver = &req[2..];
        compare_versions(ver, req_ver) != std::cmp::Ordering::Less
    } else if req.starts_with('>') {
        let req_ver = &req[1..];
        compare_versions(ver, req_ver) == std::cmp::Ordering::Greater
    } else if req.starts_with('=') {
        let req_ver = &req[1..];
        ver == req_ver
    } else {
        ver == req
    }
}

pub fn ensure_deb_packages(packages: &[String]) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }
    let output = Command::new("dpkg-query")
    .args(&["-W", "-f=${Package}\\n"])
    .output()
    .into_diagnostic()?;
    let installed = String::from_utf8(output.stdout).into_diagnostic()?;
    let installed_lines: Vec<&str> = installed.lines().collect();
    let missing: Vec<_> = packages.iter()
    .filter(|p| !installed_lines.contains(&p.as_str()))
    .collect();
    if missing.is_empty() {
        return Ok(());
    }
    println!("{} The following system packages are required:", "→".bright_black());
    for p in &missing {
        println!("  - {}", p);
    }
    print!("Install them now? [y/N] ");
    std::io::stdout().flush().into_diagnostic()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).into_diagnostic()?;
    if input.trim().eq_ignore_ascii_case("y") {
        let status = Command::new("sudo")
        .arg("apt")
        .arg("install")
        .arg("-y")
        .args(&missing)
        .status()
        .into_diagnostic()?;
        if !status.success() {
            bail!("Failed to install system packages");
        }
    } else {
        bail!("Missing system packages");
    }
    Ok(())
}

/// Rekurencyjnie kopiuje zawartość katalogu `src` do `dst` (dst musi już
/// istnieć). Używane m.in. do zachowania `hooks/` przed usunięciem pakietu,
/// żeby post-remove hook mógł się uruchomić już po skasowaniu store'u.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry.into_diagnostic()?;
        let rel = entry.path().strip_prefix(src).into_diagnostic()?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).into_diagnostic()?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).into_diagnostic()?;
            }
            fs::copy(entry.path(), &target).into_diagnostic()?;
        }
    }
    Ok(())
}
