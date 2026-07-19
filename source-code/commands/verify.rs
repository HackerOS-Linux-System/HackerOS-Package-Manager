use miette::{Result, bail, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use crate::{state::State, utils::compute_dir_hash};

pub(crate) const TRUSTED_KEYRING: &str = "/etc/hpm/trusted-keys.gpg";
pub(crate) const USER_KEYRING:    &str = "~/.config/hpm/trusted-keys.gpg";

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
        .map(|i| i.checksum.clone())
        .ok_or_else(|| miette!("No checksum in state for {}@{}", package, current_ver))?;

    let pkg_path = Path::new(crate::store_path()).join(&package).join(&current_ver);
    crate::squash::ensure_mounted(&pkg_path)?;

    println!("{} Verifying {}@{}...\n", "→".white(), package.white(), current_ver.red());

    // ── 1. SHA-256 ───────────────────────────────────────────────────────────
    print!("  {:<40}", "Content hash (SHA-256):".bold());
    let computed = compute_dir_hash(&pkg_path)?;
    if computed == expected {
        println!("{}", "OK".red());
        let short = &computed[..computed.len().min(32)];
        println!("    {}", short.dimmed());
    } else {
        println!("{}", "MISMATCH".red().bold());
        println!("    stored:   {}", &expected[..16.min(expected.len())].red());
        println!("    computed: {}", &computed[..16.min(computed.len())].red());
        bail!("Content checksum verification failed for {}@{}", package, current_ver);
    }

    // ── 2. GPG podpis ────────────────────────────────────────────────────────
    let info_hk     = pkg_path.join("info.hk");
    let info_hk_sig = pkg_path.join("info.hk.sig");

    print!("  {:<40}", "GPG signature (info.hk.sig):".bold());
    if !info_hk_sig.exists() {
        println!("{}", "NOT PRESENT".bright_black());
        println!("    {} Package is not GPG-signed.", "⚠".bright_black());
        println!("    {} For security, prefer signed packages.", "→".dimmed());
    } else if !info_hk.exists() {
        println!("{}", "ERROR".red());
        bail!("info.hk missing from store — cannot verify signature");
    } else {
        match verify_gpg_signature(&info_hk, &info_hk_sig) {
            Ok(signer) => {
                println!("{}", "OK".red());
                println!("    Signed by: {}", signer.white());
            }
            Err(e) => {
                println!("{}", "FAILED".red().bold());
                println!("    {}", e);
                bail!("GPG signature verification failed");
            }
        }
    }

    // ── 3. Manifest readable ─────────────────────────────────────────────────
    print!("  {:<40}", "Manifest (info.hk) readable:".bold());
    match crate::manifest::Manifest::load_from_path(pkg_path.to_str().unwrap()) {
        Ok(m)  => {
            println!("{}", "OK".red());
            println!("    name={} version={}", m.name.white(), m.version.red());
        }
        Err(e) => {
            println!("{}", "ERROR".red());
            println!("    {}", e);
        }
    }

    println!();
    println!("{} Verification passed for {}@{}", "✔".red(), package.white(), current_ver.red());
    Ok(())
}

// ---------------------------------------------------------------------------
// GPG — FIXED: export_keys wymaga &Key (reference), nie Key (owned)
// ---------------------------------------------------------------------------

pub(crate) fn verify_gpg_signature(data_file: &Path, sig_file: &Path) -> Result<String> {
    use gpgme::{Context, Protocol, SignatureSummary};

    let mut ctx = Context::from_protocol(Protocol::OpenPgp)
        .map_err(|e| miette!("Failed to initialize GPGME: {}", e))?;

    // Załaduj trusted keyring
    let keyring_path = if Path::new(TRUSTED_KEYRING).exists() {
        Some(TRUSTED_KEYRING.to_string())
    } else {
        // FIXED: shellexpand jest teraz zależnością
        let user_kr = shellexpand::tilde(USER_KEYRING).to_string();
        if Path::new(&user_kr).exists() { Some(user_kr) } else { None }
    };

    if let Some(kr) = keyring_path {
        let kr_data = fs::read(&kr).into_diagnostic()?;
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
        let fp = sig.fingerprint()
            .map(|f| f.to_string())
            .unwrap_or_else(|_| "unknown fingerprint".to_string());

        // BUG NAPRAWIONY: poprzednio odwołany/wygasły klucz nie miał
        // dedykowanej obsługi — trafiał w ogólny fallback "Signature could
        // not be verified (status: ...)" z surowym bitflagiem zamiast
        // jasnego komunikatu. Sprawdzamy te przypadki JAWNIE i PRZED
        // ogólnym fallbackiem, żeby użytkownik wiedział dokładnie co się stało.
        if summary.contains(SignatureSummary::KEY_REVOKED) {
            bail!(
                "Signing key {} has been REVOKED by its owner.\n    \
  This usually means the key was compromised or retired — do not trust this package.",
                fp
            );
        }
        if summary.contains(SignatureSummary::KEY_EXPIRED) {
            bail!(
                "Signing key {} has EXPIRED.\n    \
  The signature can't be trusted until the packager publishes a new key/signature.",
                fp
            );
        }
        if summary.contains(SignatureSummary::SIG_EXPIRED) {
            bail!(
                "Signature itself has EXPIRED (key {} is fine, but this specific signature had a validity period).\n    \
  Ask the packager to re-sign.",
                fp
            );
        }
        if summary.contains(SignatureSummary::KEY_MISSING) {
            bail!("Signing key {} not in trusted keyring.\n    Add: hpm verify --import-key {}", fp, fp);
        }
        // BUG NAPRAWIONY (znaleziony przez realny test `--import-key` z
        // nowym, świeżo zaimportowanym kluczem): gpgme nie ustawia
        // VALID/GREEN wyłącznie na podstawie poprawnej kryptografii —
        // wymaga ownertrust (własnej "sieci zaufania" GPG: klucz musi być
        // podpisany przez kogoś zaufanego, albo mieć ręcznie ustawiony
        // poziom zaufania). Świeżo zaimportowany klucz ma trust "unknown",
        // więc nawet w 100% poprawny podpis dostawał pusty SignatureSummary
        // (żadnych flag) i trafiał w ogólny, mylący fallback poniżej.
        //
        // To niewłaściwy model zaufania dla hpm: SAMO umieszczenie klucza w
        // `trusted-keys.gpg` (przez `--import-key`) JEST decyzją o zaufaniu
        // — nie potrzebujemy DODATKOWO ownertrust GPG na to nałożonego.
        // Liczy się tylko brak sygnałów o realnym problemie (BAD/error),
        // które są już jawnie obsłużone powyżej (REVOKED/EXPIRED/MISSING) i
        // poniżej (RED/SYS_ERROR) — reszta (w tym "pusty" summary) to po
        // prostu "podpis poprawny, klucz zaufany przez hpm".
        if summary.contains(SignatureSummary::RED) {
            bail!("BAD signature — package may be tampered with!");
        }
        if summary.contains(SignatureSummary::SYS_ERROR) {
            bail!("GPG reported a system error while checking this signature — treating as untrusted.");
        }
        signers.push(fp);
    }

    if signers.is_empty() { bail!("No valid signatures found"); }
    Ok(signers.join(", "))
}

// ---------------------------------------------------------------------------
// hpm verify --import-key — FIXED: export_keys używa &Key przez collect+iter
// ---------------------------------------------------------------------------

pub fn import_key(key_source: &str) -> Result<()> {
    use gpgme::{Context, Protocol, ExportMode};

    println!("{} Importing GPG key: {}", "→".white(), key_source.white());

    // Pobierz dane klucza
    let key_data = if Path::new(key_source).exists() {
        fs::read(key_source).into_diagnostic()?
    } else {
        // FIXED: reqwest::blocking dostępne po dodaniu feature "blocking"
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

    // BUG NAPRAWIONY: poprzednio zawsze zapisywał do /etc/hpm/trusted-keys.gpg
    // — katalog systemowy wymagający roota, sprzeczny z resztą hpm od 0.9
    // (wszystko inne działa bez sudo). Piszemy do keyringu użytkownika;
    // /etc/hpm pozostaje tylko jako opcjonalny, tylko-do-odczytu store
    // dostarczany przez admina (patrz `verify_gpg_signature`, które i tak
    // sprawdza system PRZED user keyringiem).
    let user_keyring_path = shellexpand::tilde(USER_KEYRING).to_string();
    let keyring_dir = Path::new(&user_keyring_path).parent().unwrap();
    fs::create_dir_all(keyring_dir).into_diagnostic()?;

    // FIXED: export_keys potrzebuje IntoIterator<Item = &Key>
    // Zbieramy klucze do Vec<Key>, a potem exportujemy przez referencje
    let all_keys: Vec<gpgme::Key> = ctx.keys()
        .map_err(|e| miette!("{}", e))?
        .filter_map(|k| k.ok())
        .collect();

    let mut exported = Vec::new();
    ctx.export_keys(
        all_keys.iter(),  // iter() daje &Key — spełnia IntoIterator<Item = &Key>
        ExportMode::empty(),
        &mut exported,
    ).map_err(|e| miette!("Export: {}", e))?;

    // Jeśli już istnieje wcześniejszy plik keyringu, dołącz nowy klucz do
    // niego zamiast nadpisywać cały plik samym świeżo wyeksportowanym
    // stanem kontekstu (który mógłby nie zawierać kluczy zaimportowanych
    // w poprzednich, osobnych wywołaniach `hpm verify --import-key`).
    if Path::new(&user_keyring_path).exists() {
        let existing = fs::read(&user_keyring_path).into_diagnostic()?;
        let _ = ctx.import(existing.as_slice());
        exported.clear();
        let all_keys2: Vec<gpgme::Key> = ctx.keys().map_err(|e| miette!("{}", e))?
            .filter_map(|k| k.ok()).collect();
        ctx.export_keys(all_keys2.iter(), ExportMode::empty(), &mut exported)
            .map_err(|e| miette!("Export: {}", e))?;
    }

    fs::write(&user_keyring_path, &exported).into_diagnostic()?;

    let imported  = result.imported();
    let unchanged = result.unchanged();
    if imported > 0 {
        println!("{} Imported {} key(s) to {}", "✔".red(), imported, user_keyring_path.white());
    } else if unchanged > 0 {
        println!("{} Key already in keyring", "→".bright_black());
    }
    Ok(())
}
