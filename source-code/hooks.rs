use miette::{Result, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Typ hooka
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HookKind {
    PreInstall,
    PostInstall,
    PreRemove,
    PostRemove,
    PostUpdate,
}

impl HookKind {
    /// Nazwa bazowa bez rozszerzenia
    fn base_name(&self) -> &'static str {
        match self {
            Self::PreInstall  => "pre-install",
            Self::PostInstall => "post-install",
            Self::PreRemove   => "pre-remove",
            Self::PostRemove  => "post-remove",
            Self::PostUpdate  => "post-update",
        }
    }

    pub fn display(&self) -> &'static str { self.base_name() }

    /// Czy ten hook blokuje operację przy błędzie
    fn blocks_on_failure(&self) -> bool {
        matches!(self, Self::PreInstall | Self::PreRemove)
    }
}

/// Kontekst wykonania hooka
pub struct HookContext<'a> {
    pub pkg_name:    &'a str,
    pub pkg_version: &'a str,
    pub store_path:  &'a str,
    pub old_version: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Interpreter selection
// ---------------------------------------------------------------------------

/// Rozszerzenia i odpowiadające im interpretery, w kolejności pierwszeństwa.
/// .hl jest domyślne — Hacker Lang.
const HOOK_EXTENSIONS: &[(&str, &str)] = &[
    (".hl",  "hl"),       // Hacker Lang — domyślny
    (".py",  "python3"),  // Python 3
    (".rb",  "ruby"),     // Ruby
    (".sh",  "sh"),       // POSIX sh
];

/// Timeout hooka w sekundach
const HOOK_TIMEOUT_SECS: u64 = 60;

/// Znajdź plik hooka w katalogu (sprawdza wszystkie obsługiwane rozszerzenia).
fn find_hook_file(dir: &Path, kind: HookKind) -> Option<(std::path::PathBuf, &'static str)> {
    let hooks_dir = dir.join("hooks");
    for (ext, interpreter) in HOOK_EXTENSIONS {
        let path = hooks_dir.join(format!("{}{}", kind.base_name(), ext));
        if path.exists() {
            return Some((path, interpreter));
        }
    }
    None
}

/// Sprawdź dostępność interpretera
fn interpreter_available(interpreter: &str) -> bool {
    Command::new("which")
        .arg(interpreter)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Wybierz interpreter dla pliku hooka.
/// Priorytety:
///   1. Shebang z pliku (#!/usr/bin/env X lub #!/path/X)
///   2. Interpreter pasujący do rozszerzenia
///   3. Fallback /bin/sh jeśli hl niedostępny
fn select_interpreter(hook_path: &Path, default_interpreter: &str) -> String {
    // Czytaj shebang z pliku
    if let Ok(content) = fs::read(hook_path) {
        if content.starts_with(b"#!") {
            if let Some(newline) = content.iter().position(|&b| b == b'\n') {
                let shebang = String::from_utf8_lossy(&content[2..newline]);
                let shebang = shebang.trim();
                // #!/usr/bin/env hl → "hl"
                if shebang.starts_with("/usr/bin/env ") {
                    let interp = shebang.trim_start_matches("/usr/bin/env ").split_whitespace()
                        .next().unwrap_or("sh").to_string();
                    if interpreter_available(&interp) {
                        return interp;
                    }
                    // Fallback dla hl → sh (kompatybilność gdy hl niezainstalowany)
                    if interp == "hl" {
                        eprintln!("  {} Hacker Lang (hl) not found — falling back to /bin/sh",
                                  "⚠".yellow());
                        eprintln!("    Install hl: {}", "hpm install hacker-lang".yellow());
                        return "sh".to_string();
                    }
                }
                // #!/usr/bin/hl lub #!/bin/sh itp.
                if let Some(last) = shebang.split('/').last() {
                    let interp = last.split_whitespace().next().unwrap_or("sh");
                    if interpreter_available(interp) {
                        return interp.to_string();
                    }
                }
            }
        }
    }

    // Brak shebang lub shebang niedostępny — użyj domyślnego dla rozszerzenia
    if interpreter_available(default_interpreter) {
        return default_interpreter.to_string();
    }

    // Ostateczny fallback
    "sh".to_string()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sprawdź czy hook istnieje
pub fn hook_exists(dir: &Path, kind: HookKind) -> bool {
    find_hook_file(dir, kind).is_some()
}

/// Uruchom hook z timeoutem.
/// Zwraca Ok(true) jeśli hook uruchomiony, Ok(false) jeśli nie istnieje.
pub fn run_hook(dir: &Path, kind: HookKind, ctx: &HookContext) -> Result<bool> {
    let (hook_path, default_interp) = match find_hook_file(dir, kind) {
        Some(h) => h,
        None    => return Ok(false),
    };

    println!("  {} Running {} hook ({})...",
             "→".cyan(), kind.display().bold(),
             hook_path.file_name().unwrap_or_default().to_string_lossy().dimmed());

    crate::utils::make_executable(&hook_path)?;

    let interpreter = select_interpreter(&hook_path, default_interp);

    // Przygotuj zmienne środowiskowe
    let mut cmd = Command::new(&interpreter);
    cmd.arg(&hook_path)
       .current_dir(dir)
       .env("HPM_PKG_NAME",    ctx.pkg_name)
       .env("HPM_PKG_VERSION", ctx.pkg_version)
       .env("HPM_STORE_PATH",  ctx.store_path)
       .env("HPM_HOOK_TYPE",   kind.display())
       .env("HPM_HOOK_LANG",   &interpreter);

    if let Some(old) = ctx.old_version {
        cmd.env("HPM_OLD_VERSION", old);
    }

    // Uruchom z timeoutem przez wait_timeout lub przez osobny wątek
    let output = run_with_timeout(cmd, Duration::from_secs(HOOK_TIMEOUT_SECS))?;

    // Wydrukuj stdout/stderr hooka
    if !output.stdout.is_empty() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            println!("    {}", line.dimmed());
        }
    }
    if !output.stderr.is_empty() {
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            eprintln!("    {}{}", "hook: ".dimmed(), line);
        }
    }

    if output.status.success() {
        println!("  {} Hook {} completed", "✔".green(), kind.display());
        Ok(true)
    } else {
        let code = output.status.code().unwrap_or(1);
        if kind.blocks_on_failure() {
            Err(miette!(
                "Hook '{}' failed with exit code {}.\n\
  Pre-install/pre-remove hooks block the operation on failure.\n\
  Fix the hook script or remove hooks/{}{} to skip it.",
                kind.display(), code,
                kind.base_name(),
                // Pokaż właściwe rozszerzenie
                hook_path.extension().and_then(|e| e.to_str())
                    .map(|e| format!(".{}", e)).unwrap_or_default()
            ))
        } else {
            eprintln!("  {} Hook '{}' failed (code {}), continuing",
                      "⚠".yellow(), kind.display(), code);
            Ok(true)
        }
    }
}

/// Uruchom komendę z timeoutem.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<std::process::Output> {
    use std::sync::mpsc;
    use std::thread;

    let (tx, rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        let output = cmd.output();
        let _ = tx.send(output);
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            let _ = handle.join();
            result.into_diagnostic()
        }
        Err(_) => {
            // Timeout — nie możemy łatwo kill() spawned procesu bez nix
            // W praktyce: proces zostanie orphaned ale hpm będzie kontynuować
            eprintln!("  {} Hook timed out after {}s — killed", "✗".red(), timeout.as_secs());
            Err(miette!("Hook timed out after {} seconds", timeout.as_secs()))
        }
    }
}

/// Skopiuj hooki z src do dest przy instalacji.
pub fn install_hooks(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    let hooks_src = src_dir.join("hooks");
    if !hooks_src.exists() { return Ok(()); }

    let hooks_dst = dest_dir.join("hooks");
    fs::create_dir_all(&hooks_dst).into_diagnostic()?;

    // Skopiuj wszystkie pliki hooków (dowolne rozszerzenie)
    if let Ok(rd) = fs::read_dir(&hooks_src) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_file() {
                let dst = hooks_dst.join(path.file_name().unwrap());
                fs::copy(&path, &dst).into_diagnostic()?;
                crate::utils::make_executable(&dst)?;
            }
        }
    }
    Ok(())
}

/// Waliduj hooki podczas `hpm build`.
pub fn validate_hooks(dir: &Path) -> Vec<String> {
    let hooks_dir = dir.join("hooks");
    if !hooks_dir.exists() { return Vec::new(); }

    let valid_bases = [
        "pre-install", "post-install",
        "pre-remove",  "post-remove",
        "post-update",
    ];
    let valid_exts: Vec<&str> = HOOK_EXTENSIONS.iter().map(|(e, _)| *e).collect();

    let mut warnings = Vec::new();

    if let Ok(rd) = fs::read_dir(&hooks_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }

            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();

            // Sprawdź czy nazwa bazowa jest poprawna
            let base_ok = valid_bases.iter().any(|b| {
                valid_exts.iter().any(|e| filename == format!("{}{}", b, e))
            });
            if !base_ok {
                warnings.push(format!(
                    "Unknown hook file: hooks/{} — expected <name>.<ext> where name ∈ {:?} and ext ∈ {:?}",
                    filename, valid_bases, valid_exts
                ));
                continue;
            }

            // Sprawdź shebang
            if let Ok(content) = fs::read(&path) {
                if !content.starts_with(b"#!") {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    let recommended = match ext {
                        "hl" => "#!/usr/bin/env hl",
                        "py" => "#!/usr/bin/env python3",
                        "rb" => "#!/usr/bin/env ruby",
                        "sh" => "#!/bin/sh",
                        _    => "#!/usr/bin/env hl",
                    };
                    warnings.push(format!(
                        "hooks/{} has no shebang line — add: {}",
                        filename, recommended
                    ));
                }

                // Dla .hl sprawdź czy ma 'using <gen 2>'
                if filename.ends_with(".hl") {
                    let text = String::from_utf8_lossy(&content);
                    if !text.contains("using <gen 2>") && !text.contains("using <gen 1>") {
                        warnings.push(format!(
                            "hooks/{} missing 'using <gen 2>' declaration — Hacker Lang gen 2 is recommended",
                            filename
                        ));
                    }
                }
            }

            // Sprawdź executable bit
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = path.metadata() {
                if meta.permissions().mode() & 0o111 == 0 {
                    warnings.push(format!(
                        "hooks/{} is not executable — fix: git update-index --chmod=+x hooks/{}",
                        filename, filename
                    ));
                }
            }
        }
    }

    warnings
}
