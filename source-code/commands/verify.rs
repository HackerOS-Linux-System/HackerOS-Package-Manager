use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{STORE_PATH, state::State, utils::compute_dir_hash};

const TRUSTED_KEYRING: &str = "/etc/hpm/trusted-keys.gpg";
const USER_KEYRING:    &str = "~/.config/hpm/trusted-keys.gpg";

pub fn verify(package: String) -> Result<()> {
    if package.is_empty() {
        eprintln!("{} Usage: hpm verify <package>", "✗".red());
        std::process::exit(1);
    }

    let state       = State::load()?;
    let current_ver = state.get_current_version(&package)
        .ok_or_else(|| miette!("Package '{}' not installed", package))?;
    let expected    = state.packages.get(&package)
        .and_then(|vs| vs.get(&current_ver))
        .map(|info| info.checksum.clone())
        .ok_or_else(|| miette!("No checksum in state for {}@{}", package, current_ver))?;

    let pkg_path = Path::new(STORE_PATH).join(&package).join(&current_ver);

    println!("{} Verifying {}@{}...\n", "→".cyan(), package.cyan(), current_ver.green());

    // ── 1. SHA-256 hash zawartości ───────────────────────────────────────────
    print!("  {:<40}", "Content hash (SHA-256):".bold());
    let computed = compute_dir_hash(&pkg_path)?;
    if computed == expected {
        println!("{}", "OK".green());
        println!("    {}", &computed[..32].dimmed());
    } else {
        println!("{}", "MISMATCH".red().bold());
        println!("    stored:   {}", &expected[..32].red());
        println!("    computed: {}", &computed[..32].red());
        bail!("Content checksum verification failed for {}@{}", package, current_ver);
    }

    // ── 2. GPG podpis info.hk ────────────────────────────────────────────────
    let info_hk     = pkg_path.join("info.hk");
    let info_hk_sig = pkg_path.join("info.hk.sig");

    print!("  {:<40}", "GPG signature (info.hk.sig):".bold());

    if !info_hk_sig.exists() {
        println!("{}", "NOT PRESENT".yellow());
        println!("    {} Package is not GPG-signed.", "⚠".yellow());
        println!("    {} For security, prefer signed packages from verified authors.", "→".dimmed());
    } else if !info_hk.exists() {
        println!("{}", "ERROR".red());
        println!("    info.hk missing from store — cannot verify signature.");
        bail!("info.hk missing");
    } else {
        match verify_gpg_signature(&info_hk, &info_hk_sig) {
            Ok(signer) => {
                println!("{}", "OK".green());
                println!("    Signed by: {}", signer.cyan());
            }
            Err(e) => {
                println!("{}", "FAILED".red().bold());
                println!("    {}", e);
                bail!("GPG signature verification failed");
            }
        }
    }

    // ── 3. Manifest integrity ─────────────────────────────────────────────────
    print!("  {:<40}", "Manifest (info.hk) readable:".bold());
    match crate::manifest::Manifest::load_from_path(pkg_path.to_str().unwrap()) {
        Ok(manifest) => {
            println!("{}", "OK".green());
            println!("    name={} version={}", manifest.name.cyan(), manifest.version.green());
        }
        Err(e) => {
            println!("{}", "ERROR".red());
            println!("    {}", e);
        }
    }

    println!();
    println!("{} Verification passed for {}@{}", "✔".green(), package.cyan(), current_ver.green());
    Ok(())
}

// ---------------------------------------------------------------------------
// GPG verification via gpgme crate
// ---------------------------------------------------------------------------

fn verify_gpg_signature(data_file: &Path, sig_file: &Path) -> Result<String> {
    use gpgme::{Context, Protocol, SignatureSummary};

    let mut ctx = Context::from_protocol(Protocol::OpenPgp)
        .map_err(|e| miette!("Failed to initialize GPGME: {}", e))?;

    // Załaduj trusted keyring jeśli istnieje
    let keyring_path = if Path::new(TRUSTED_KEYRING).exists() {
        Some(TRUSTED_KEYRING)
    } else {
        let user_kr = shellexpand::tilde(USER_KEYRING).to_string();
        if Path::new(&user_kr).exists() { Some(user_kr.as_str()) } else { None }
    };

    if let Some(kr) = keyring_path {
        // Import kluczy z keyring
        let kr_data = fs::read(kr).into_diagnostic()?;
        ctx.import(kr_data.as_slice())
            .map_err(|e| miette!("Failed to import keyring: {}", e))?;
    }

    let data_bytes = fs::read(data_file).into_diagnostic()?;
    let sig_bytes  = fs::read(sig_file).into_diagnostic()?;

    let result = ctx.verify_detached(sig_bytes.as_slice(), data_bytes.as_slice())
        .map_err(|e| miette!("GPG verification error: {}", e))?;

    let mut signers = Vec::new();
    for sig in result.signatures() {
        let summary = sig.summary();
        // Sprawdź czy podpis jest ważny
        if summary.contains(SignatureSummary::VALID) || summary.contains(SignatureSummary::GREEN) {
            let signer = sig.fingerprint()
                .map(|f| f.to_string())
                .unwrap_or_else(|_| "unknown fingerprint".to_string());
            signers.push(signer);
        } else if summary.contains(SignatureSummary::KEY_MISSING) {
            bail!("Signing key not in trusted keyring.\n    Add it with: hpm verify --import-key <keyid>");
        } else if summary.contains(SignatureSummary::RED) {
            bail!("BAD signature — package may be tampered with!");
        } else {
            bail!("Signature could not be verified (status: {:?})", summary);
        }
    }

    if signers.is_empty() {
        bail!("No valid signatures found");
    }

    Ok(signers.join(", "))
}

// ---------------------------------------------------------------------------
// hpm verify --import-key <keyid|keyfile>
// ---------------------------------------------------------------------------

pub fn import_key(key_source: &str) -> Result<()> {
    use gpgme::{Context, Protocol};

    println!("{} Importing GPG key: {}", "→".cyan(), key_source.cyan());

    let key_data = if Path::new(key_source).exists() {
        // Plik lokalny
        fs::read(key_source).into_diagnostic()?
    } else {
        // Pobierz z serwera kluczy
        let url = if key_source.starts_with("http") {
            key_source.to_string()
        } else {
            format!("https://keyserver.ubuntu.com/pks/lookup?op=get&search={}", key_source)
        };
        let response = reqwest::blocking::get(&url)
            .map_err(|e| miette!("Failed to fetch key: {}", e))?;
        response.bytes().into_diagnostic()?.to_vec()
    };

    let mut ctx = Context::from_protocol(Protocol::OpenPgp)
        .map_err(|e| miette!("GPGME: {}", e))?;

    let result = ctx.import(key_data.as_slice())
        .map_err(|e| miette!("Import failed: {}", e))?;

    // Zapisz do trusted keyring
    let keyring_dir = Path::new(TRUSTED_KEYRING).parent().unwrap();
    fs::create_dir_all(keyring_dir).into_diagnostic()?;

    // Eksportuj zaktualizowany keyring
    let mut exported = Vec::new();
    ctx.export_keys(
        ctx.keys().map_err(|e| miette!("{}", e))?
           .filter_map(|k| k.ok()),
        gpgme::ExportMode::empty(),
        &mut exported,
    ).map_err(|e| miette!("Export: {}", e))?;
    fs::write(TRUSTED_KEYRING, &exported).into_diagnostic()?;

    let imported = result.imported();
    let unchanged = result.unchanged();
    if imported > 0 {
        println!("{} Imported {} key(s) to {}", "✔".green(), imported, TRUSTED_KEYRING.cyan());
    } else if unchanged > 0 {
        println!("{} Key already in keyring ({})", "→".yellow(), TRUSTED_KEYRING.dimmed());
    }
    Ok(())
}
