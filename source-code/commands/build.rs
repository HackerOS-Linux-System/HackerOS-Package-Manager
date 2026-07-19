use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use std::process::Command;

/// `hpm build [name] [--output <path>] [--sign <key-id>]`
///
/// Pakuje bieżący katalog pakietu (info.hk + contents/ + hooks/ + build.toml)
/// w dystrybuowalne archiwum `.hpm` — odpowiednik `makepkg` z pacmana, tylko
/// dla hpm. Efekt tej komendy to dokładnie to, co potem konsumuje
/// `hpm install --release` (patrz `commands::install`), pobierając `.hpm` z
/// GitHub Releases zamiast klonować całe repo git.
///
/// Domyślnie generuje też `<output>.sha256` (suma kontrolna, zawsze) i,
/// opcjonalnie z `--sign <key-id>`, `<output>.sig` (odłączony podpis GPG,
/// weryfikowalny później przez `hpm verify`) — to jest odpowiednik podpisów
/// pakietów w pacmanie/`.pkg.tar.zst.sig`.
pub fn build(args: Vec<String>) -> Result<()> {
    let mut name:    String = String::new();
    let mut output:  Option<String> = None;
    let mut sign_key: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" | "-o" => {
                i += 1;
                output = args.get(i).cloned();
                if output.is_none() { bail!("--output requires a path argument"); }
            }
            "--sign" => {
                i += 1;
                sign_key = args.get(i).cloned();
                if sign_key.is_none() { bail!("--sign requires a GPG key id / fingerprint argument"); }
            }
            other if name.is_empty() && !other.starts_with('-') => { name = other.to_string(); }
            other => { eprintln!("  {} Unknown build argument: {}", "⚠".bright_black(), other); }
        }
        i += 1;
    }

    println!("{} Building hpm package...\n", "→".white());

    // ── 1. Walidacja info.hk ─────────────────────────────────────────────────
    let info_hk = Path::new("info.hk");
    if !info_hk.exists() {
        bail!(
            "info.hk not found in current directory.\n\
  Run {} to create a new package from scratch.",
            "hpm create".bright_black()
        );
    }

    print!("  {:<40}", "Validating info.hk:".bold());
    let manifest = crate::manifest::Manifest::load_from_path(".")
        .map_err(|e| miette!("info.hk validation failed: {}", e))?;
    println!("{}", "OK".red());

    // Sprawdź wymagane pola
    validate_manifest(&manifest)?;

    // ── 2. Sprawdź strukturę pakietu ──────────────────────────────────────────
    print!("  {:<40}", "Package structure:".bold());
    let has_contents  = Path::new("contents").exists();
    let has_build_toml = Path::new("build.toml").exists();

    if !has_contents && !has_build_toml {
        println!("{}", "ERROR".red().bold());
        bail!(
            "Package needs either contents/ directory or build.toml\n\
  contents/ — pre-built binary files\n\
  build.toml — instructions to download or build from source"
        );
    }
    println!("{}", "OK".red());

    // ── 3. Sprawdź czy binaria są wykonywalne ─────────────────────────────────
    if has_contents {
        print!("  {:<40}", "Binary executability:".bold());
        let mut not_exec = Vec::new();
        for bin_name in &manifest.bins {
            let candidates = [
                format!("contents/bin/{}", bin_name),
                format!("contents/{}", bin_name),
            ];
            for path in &candidates {
                let p = Path::new(path);
                if p.exists() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = fs::metadata(p).into_diagnostic()?.permissions().mode();
                    if mode & 0o111 == 0 {
                        not_exec.push(path.clone());
                    }
                    break;
                }
            }
        }
        if not_exec.is_empty() {
            println!("{}", "OK".red());
        } else {
            println!("{}", "WARNING".bright_black());
            for path in &not_exec {
                println!("    {} {} is not executable", "⚠".bright_black(), path);
                println!("      Fix: {}", format!("git update-index --chmod=+x {}", path).white());
            }
        }
    }

    // ── 3b. Sprawdź przenośność (biblioteki współdzielone spoza contents/) ────
    // Spakowanie działającego binarki na maszynie budującej NIE znaczy że
    // zadziała u odbiorcy — jeśli binarka jest dynamicznie linkowana i
    // wskazuje na .so w niestandardowej ścieżce (np. lokalny toolchain,
    // conda env, /home/builder/...), ta biblioteka może po prostu nie
    // istnieć na maszynie docelowej. Standardowe ścieżki systemowe (/usr/lib,
    // /lib itd.) są bind-mountowane do sandboxa (patrz sandbox.rs) więc te są
    // bezpieczne — ostrzegamy tylko o tym, co wykracza poza nie.
    print!("  {:<40}", "Portability (shared libs):".bold());
    let mut portability_warnings = Vec::new();
    for bin_name in &manifest.bins {
        let candidates = [
            format!("contents/bin/{}", bin_name),
            format!("contents/{}", bin_name),
        ];
        for path in &candidates {
            let p = Path::new(path);
            if p.exists() {
                check_binary_portability(p, bin_name, &mut portability_warnings);
                break;
            }
        }
    }
    if portability_warnings.is_empty() {
        println!("{}", "OK".red());
    } else {
        println!("{}", "WARNING".bright_black());
        for w in &portability_warnings {
            println!("    {} {}", "⚠".bright_black(), w);
        }
        println!("    These are dynamic library dependencies OUTSIDE contents/ and outside");
        println!("    standard system library paths — they must also exist on the recipient's");
        println!("    machine, or the binary won't run. Consider static linking (e.g. musl),");
        println!("    or bundling the .so files under contents/ next to the binary.");
    }

    // ── 4. Sprawdź spójność tagów ─────────────────────────────────────────────
    if !manifest.tags.is_empty() {
        print!("  {:<40}", "Group tags:".bold());
        let valid_chars = manifest.tags.iter().all(|t| {
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
        if valid_chars {
            println!("{} [{}]", "OK".red(),
                manifest.tags.iter().map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", ").dimmed());
        } else {
            println!("{}", "WARNING".bright_black());
            println!("    Tags should only contain letters, digits, hyphens and underscores.");
        }
    }

    // ── 5. Sprawdź czy wersja pasuje do tagu git ──────────────────────────────
    print!("  {:<40}", "Git tag consistency:".bold());
    let git_tag = get_latest_git_tag();
    match &git_tag {
        Some(tag) => {
            let tag_ver = tag.trim_start_matches('v');
            if tag_ver == manifest.version {
                println!("{} [{}]", "OK".red(), tag.dimmed());
            } else {
                println!("{}", "WARNING".bright_black());
                println!("    Latest git tag: {} but info.hk version: {}",
                         tag.bright_black(), manifest.version.white());
                println!("    Create tag: {}", format!("git tag v{}", manifest.version).white());
            }
        }
        None => {
            println!("{}", "NO TAGS".bright_black());
            println!("    Create a release tag: {}", format!("git tag v{}", manifest.version).white());
        }
    }

    // ── 6. Nazwa archiwum ────────────────────────────────────────────────────
    let pkg_name = if name.is_empty() {
        format!("{}-{}", manifest.name, manifest.version)
    } else {
        name
    };
    let output = output.unwrap_or_else(|| format!("{}.hpm", pkg_name));

    // ── 7. Sprawdź arch ──────────────────────────────────────────────────────
    print!("  {:<40}", "Architecture check:".bold());
    let current_arch = std::env::consts::ARCH;
    let declared_arch = manifest.system_specs.get("arch")
        .map(|s| s.as_str())
        .unwrap_or("any");
    if declared_arch == "any" || declared_arch == current_arch
        || (declared_arch == "x86_64" && current_arch == "x86_64")
        || (declared_arch == "aarch64" && current_arch == "aarch64")
    {
        println!("{} [{}]", "OK".red(), declared_arch.dimmed());
    } else {
        println!("{}", "MISMATCH".bright_black());
        println!("    Package declares arch={} but building on {}", declared_arch, current_arch);
    }

    // ── 8. Pakowanie ──────────────────────────────────────────────────────────
    println!();

    // BUG NAPRAWIONY (znaleziony przy pakowaniu prawdziwego pakietu): `hpm
    // build` zawsze zakładał `zstd` w PATH i po prostu wywalał się z gołym
    // "tar: Child returned status 127", jeśli go brakowało — bez żadnej
    // podpowiedzi co robić. `tar -xf` przy WYPAKOWYWANIU (patrz
    // `install_single_from_release`) i tak autodetekuje kompresję po
    // zawartości pliku, nie po rozszerzeniu, więc spadek do gzip jest w pełni
    // bezpieczny i przezroczysty dla odbiorcy .hpm — nie musi mieć zstd.
    let have_zstd = Command::new("which").arg("zstd").output()
        .map(|o| o.status.success()).unwrap_or(false);
    let compress_args: Vec<&str> = if have_zstd { vec!["-I", "zstd"] } else { vec!["-z"] };
    print!("  {:<40}", format!("Packaging → {} ({}):", output,
                                if have_zstd { "zstd" } else { "gzip — zstd not found in PATH" }).bold());
    if !have_zstd {
        println!();
        println!("    {} Install {} for smaller archives (optional): {}",
                  "ℹ".white(), "zstd".white(), "apt install zstd".dimmed());
        print!("  {:<40}", "".bold());
    }

    // Wyklucz pliki deweloperskie
    let mut tar_args: Vec<&str> = compress_args;
    // Reprodukowalność: ten sam katalog źródłowy powinien dać bitowo
    // identyczny .hpm niezależnie od tego, KTO buduje, KIEDY, i jakim
    // userem. Bez tego GNU tar wbudowuje mtime/uid/gid/kolejność wpisów z
    // filesystemu budującego, więc dwa identyczne buildy tego samego commitu
    // dają różne pliki — uniemożliwia to niezależną weryfikację "ten kod
    // źródłowy = ten właśnie .hpm" (ważne obok podpisu GPG: podpis mówi kto
    // to zbudował, reprodukowalność pozwala każdemu sprawdzić CZY to, co
    // podpisano, faktycznie odpowiada publicznemu źródłu).
    tar_args.extend([
        "--sort=name",
        "--mtime=@0",
        "--owner=0", "--group=0", "--numeric-owner",
        "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
    ]);
    tar_args.extend([
        "--exclude=.git",
        "--exclude=.github",
        "--exclude=target",
        "--exclude=_build",
        "--exclude=*.o",
        "--exclude=*.d",
        "--exclude=*.tmp",
        "--exclude=.staging-*",
        "-cf", &output,
        ".",
    ]);
    let status = Command::new("tar")
        .args(&tar_args)
        .status()
        .into_diagnostic()?;

    if status.success() {
        println!("{}", "OK".red());
    } else {
        println!("{}", "FAILED".red());
        bail!("tar packaging failed (tried {})", if have_zstd { "zstd" } else { "gzip" });
    }

    // Rozmiar archiwum
    if let Ok(meta) = fs::metadata(&output) {
        let size = meta.len();
        let human = if size > 1_048_576 {
            format!("{:.1} MB", size as f64 / 1_048_576.0)
        } else {
            format!("{:.1} KB", size as f64 / 1024.0)
        };
        println!("  {:<40} {}", "Archive size:".bold(), human.red());
    }

    // ── 8b. Suma kontrolna sha256 — zawsze, jak .pkg.tar.zst w pacmanie ──────
    print!("  {:<40}", "SHA-256 checksum:".bold());
    let checksum = crate::utils::compute_file_hash(Path::new(&output))?;
    let checksum_path = format!("{}.sha256", output);
    fs::write(&checksum_path, format!("{}  {}\n", checksum, output)).into_diagnostic()?;
    println!("{}", "OK".red());
    println!("  {:<40} {}", "".bold(), checksum_path.dimmed());

    // ── 8c. Opcjonalny podpis GPG (--sign <key-id>) — jak .sig w pacmanie ────
    let mut sig_path: Option<String> = None;
    if let Some(key_id) = &sign_key {
        print!("  {:<40}", format!("GPG signature ({}):", key_id).bold());
        match sign_gpg_detached(Path::new(&output), key_id) {
            Ok(path) => {
                println!("{}", "OK".red());
                println!("  {:<40} {}", "".bold(), path.dimmed());
                sig_path = Some(path);
            }
            Err(e) => {
                println!("{}", "FAILED".red());
                println!("    {} {}", "⚠".bright_black(), e);
                println!("    Package built successfully, but unsigned — continuing.");
            }
        }
    }

    // ── 9. Podsumowanie ──────────────────────────────────────────────────────
    println!();
    println!("{} Package built: {}", "✔".red(), output.white());
    println!();
    println!("{}", "Package summary:".bold());
    println!("  name    = {}", manifest.name.white());
    println!("  version = {}", manifest.version.red());
    println!("  author  = {}", manifest.authors.dimmed());
    println!("  license = {}", manifest.license.dimmed());
    if !manifest.bins.is_empty() {
        println!("  bins    = {}", manifest.bins.join(", ").dimmed());
    }
    if !manifest.tags.is_empty() {
        println!("  tags    = {}", manifest.tags.iter()
            .map(|t| format!("@{}", t)).collect::<Vec<_>>().join(", ").dimmed());
    }
    println!();
    println!("{}", "Next steps:".bold());
    println!("  1. {}", format!("git tag v{} && git push origin main --tags", manifest.version).white());
    println!("  2. Create a GitHub Release for tag {} and attach:", format!("v{}", manifest.version).white());
    println!("       {}", output.dimmed());
    println!("       {}", checksum_path.dimmed());
    if let Some(sig) = &sig_path {
        println!("       {}", sig.dimmed());
    }
    println!("     Then anyone can run:");
    println!("       {}", format!("hpm install {} --release", manifest.name).white());
    println!("     to fetch this exact .hpm from your Releases instead of cloning the repo.");
    println!("  3. Submit a PR to the HPM package index to register your package.");
    println!("     The maintainer will add your repo URL to repo.json (unchanged format —");
    println!("     just the git repo URL; hpm figures out whether to use git or a release .hpm).");
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest validation
// ---------------------------------------------------------------------------

fn validate_manifest(manifest: &crate::manifest::Manifest) -> Result<()> {
    let mut errors   = Vec::new();
    let mut warnings = Vec::new();

    // Wymagane pola
    if manifest.name.is_empty()    { errors.push("name is empty"); }
    if manifest.version.is_empty() { errors.push("version is empty"); }
    if manifest.authors.is_empty() { warnings.push("authors is empty"); }
    if manifest.license.is_empty() { warnings.push("license is empty"); }
    if manifest.summary.is_empty() { warnings.push("summary is empty (shown in hpm search)"); }

    // Walidacja nazwy pakietu
    if !manifest.name.is_empty() {
        if !manifest.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            errors.push("name contains invalid characters (only a-z, 0-9, -, _ allowed)");
        }
        if manifest.name.starts_with('-') || manifest.name.starts_with('_') {
            errors.push("name must start with a letter or digit");
        }
    }

    // Walidacja wersji (semver-like)
    if !manifest.version.is_empty() {
        let parts: Vec<&str> = manifest.version.split('.').collect();
        if parts.len() < 2 {
            warnings.push("version should follow semver (e.g. 1.0.0)");
        } else if !parts.iter().all(|p| p.parse::<u32>().is_ok() || p.contains('-')) {
            warnings.push("version parts should be numeric");
        }
    }

    // GUI bez desktop sekcji
    if manifest.is_gui && manifest.desktop.display_name.is_empty() {
        warnings.push("GUI package has no [desktop] display_name");
    }

    // Bins ale brak summary
    if !manifest.bins.is_empty() && manifest.summary.is_empty() {
        warnings.push("package has bins but no summary — users won't see description in hpm search");
    }

    // Sprawdź arch w specs
    if let Some(arch) = manifest.system_specs.get("arch") {
        if !["x86_64", "aarch64", "armhf", "i386", "any"].contains(&arch.as_str()) {
            warnings.push("unknown arch value in [specs] — supported: x86_64, aarch64, armhf, i386, any");
        }
    }

    // Pokaż błędy i ostrzeżenia
    for w in &warnings {
        println!("  {} {}", "⚠".bright_black(), w);
    }

    if !errors.is_empty() {
        println!();
        for e in &errors {
            println!("  {} {}", "✗".red(), e);
        }
        bail!("info.hk validation failed with {} error(s)", errors.len());
    }

    Ok(())
}

/// Standardowe ścieżki bibliotek systemowych — bind-mountowane read-only do
/// sandboxa (patrz `sandbox::setup_mounts`), więc zależności stąd są
/// bezpieczne na każdej maszynie z podobną dystrybucją.
const STANDARD_LIB_DIRS: &[&str] = &[
    "/lib", "/lib64", "/lib32", "/usr/lib", "/usr/lib64", "/usr/lib32",
];

/// Uruchamia `ldd` na binarce i zbiera ostrzeżenia o zależnościach, które:
///   a) w ogóle nie zostały znalezione (`=> not found`) — złamane już TERAZ,
///      na maszynie budującej, więc na pewno złamane też u odbiorcy;
///   b) rozwiązują się do ścieżki spoza `STANDARD_LIB_DIRS` — istnieją na tej
///      maszynie, ale nie ma gwarancji że będą u odbiorcy (lokalny toolchain,
///      conda/nix env, katalog domowy budującego, itp.).
/// Statycznie linkowane binarki (ldd mówi "not a dynamic executable") i
/// nie-ELF pliki (skrypty) są cicho pomijane — nie ma dla nich czego sprawdzać.
fn check_binary_portability(path: &Path, bin_name: &str, warnings: &mut Vec<String>) {
    let Ok(data) = fs::read(path) else { return };
    if data.len() < 4 || &data[0..4] != b"\x7fELF" {
        return; // skrypt (.sh/.py/itp.) albo coś nie-ELF — nic do sprawdzenia
    }

    let Ok(output) = Command::new("ldd").arg(path).output() else {
        return; // brak `ldd` w systemie budującym — nie blokuj builda o to
    };
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains("not a dynamic executable") {
        return; // statycznie linkowane — z definicji przenośne
    }

    for line in text.lines() {
        let line = line.trim();
        if line.contains("=> not found") {
            let libname = line.split_whitespace().next().unwrap_or(line);
            warnings.push(format!(
                "{}: missing dependency '{}' — broken even on THIS machine, will definitely fail for users",
                bin_name, libname
            ));
            continue;
        }
        if let Some(idx) = line.find("=> ") {
            let resolved = line[idx + 3..].split(" (").next().unwrap_or("").trim();
            if resolved.is_empty() || resolved == "not found" {
                continue;
            }
            let is_standard = STANDARD_LIB_DIRS.iter().any(|d| resolved.starts_with(d));
            if !is_standard {
                warnings.push(format!(
                    "{}: links against '{}' from a non-standard path — may not exist on the target machine",
                    bin_name, resolved
                ));
            }
        }
    }
}

fn get_latest_git_tag() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    } else {
        None
    }
}

/// Odłączony podpis GPG dla archiwum `.hpm` — analogicznie do `.sig` przy
/// pakietach pacmana. Weryfikowalny później przez `hpm verify --import-key`
/// + weryfikację podpisu (patrz `commands::verify::verify_gpg_signature`,
/// która czyta dokładnie ten sam format: surowe bajty podpisu OpenPGP).
fn sign_gpg_detached(archive: &Path, key_id: &str) -> Result<String> {
    use gpgme::{Context, Protocol};

    let mut ctx = Context::from_protocol(Protocol::OpenPgp)
        .map_err(|e| miette!("Failed to initialize GPGME: {}", e))?;
    ctx.set_armor(false);

    let key = ctx.get_secret_key(key_id)
        .map_err(|e| miette!("Secret key '{}' not found in your GPG keyring: {}", key_id, e))?;
    ctx.add_signer(&key)
        .map_err(|e| miette!("Failed to select signing key: {}", e))?;

    let plaintext = fs::read(archive).into_diagnostic()?;
    let mut signature = Vec::new();
    ctx.sign_detached(plaintext.as_slice(), &mut signature)
        .map_err(|e| miette!("GPG signing failed: {}", e))?;

    let sig_path = format!("{}.sig", archive.display());
    fs::write(&sig_path, signature).into_diagnostic()?;
    Ok(sig_path)
}
