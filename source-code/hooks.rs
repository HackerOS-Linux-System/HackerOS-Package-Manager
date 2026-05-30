use miette::{Result, miette, IntoDiagnostic};
use colored::Colorize;
use std::fs;
use std::path::Path;
use std::process::Command;

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
    fn filename(&self) -> &'static str {
        match self {
            Self::PreInstall  => "pre-install.sh",
            Self::PostInstall => "post-install.sh",
            Self::PreRemove   => "pre-remove.sh",
            Self::PostRemove  => "post-remove.sh",
            Self::PostUpdate  => "post-update.sh",
        }
    }

    fn display(&self) -> &'static str {
        match self {
            Self::PreInstall  => "pre-install",
            Self::PostInstall => "post-install",
            Self::PreRemove   => "pre-remove",
            Self::PostRemove  => "post-remove",
            Self::PostUpdate  => "post-update",
        }
    }
}

/// Kontekst wykonania hooka
pub struct HookContext<'a> {
    pub pkg_name:    &'a str,
    pub pkg_version: &'a str,
    pub store_path:  &'a str,
    /// Poprzednia wersja (tylko dla PostUpdate)
    pub old_version: Option<&'a str>,
}

/// Sprawdź czy hook istnieje w katalogu pakietu lub store.
pub fn hook_exists(dir: &Path, kind: HookKind) -> bool {
    dir.join("hooks").join(kind.filename()).exists()
}

/// Uruchom hook jeśli istnieje.
/// Zwraca Ok(true) jeśli hook był uruchomiony, Ok(false) jeśli nie istnieje.
pub fn run_hook(dir: &Path, kind: HookKind, ctx: &HookContext) -> Result<bool> {
    let hook_path = dir.join("hooks").join(kind.filename());
    if !hook_path.exists() {
        return Ok(false);
    }

    println!("  {} Running {} hook...", "→".cyan(), kind.display().bold());

    // Ustaw uprawnienia wykonywania
    crate::utils::make_executable(&hook_path)?;

    // Przygotuj zmienne środowiskowe dla hooka
    let mut env_vars = vec![
        ("HPM_PKG_NAME",    ctx.pkg_name.to_string()),
        ("HPM_PKG_VERSION", ctx.pkg_version.to_string()),
        ("HPM_STORE_PATH",  ctx.store_path.to_string()),
        ("HPM_HOOK_TYPE",   kind.display().to_string()),
    ];
    if let Some(old) = ctx.old_version {
        env_vars.push(("HPM_OLD_VERSION", old.to_string()));
    }

    // Uruchom hook przez sh w katalogu pakietu
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(hook_path.to_str().unwrap())
       .current_dir(dir);

    for (key, val) in &env_vars {
        cmd.env(key, val);
    }

    // Timeout: hooki mają max 60 sekund
    let output = cmd.output().into_diagnostic()?;

    // Wydrukuj stdout/stderr hooka
    if !output.stdout.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            println!("    {}", line.dimmed());
        }
    }
    if !output.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            eprintln!("    {} {}", "hook:".dimmed(), line);
        }
    }

    if output.status.success() {
        println!("  {} Hook {} completed", "✔".green(), kind.display());
        Ok(true)
    } else {
        let code = output.status.code().unwrap_or(1);
        // Hooki post-* nie blokują operacji przy błędzie (tylko ostrzeżenie)
        // Hooki pre-* blokują — zwróć błąd
        match kind {
            HookKind::PreInstall | HookKind::PreRemove => {
                Err(miette!(
                    "Hook '{}' failed with exit code {}.\n\
  Pre-install/pre-remove hooks block the operation on failure.\n\
  Fix the hook script or remove hooks/{} to skip it.",
                    kind.display(), code, kind.filename()
                ))
            }
            _ => {
                eprintln!("  {} Hook '{}' failed (code {}), continuing anyway",
                          "⚠".yellow(), kind.display(), code);
                Ok(true)
            }
        }
    }
}

/// Skopiuj hooki z źródła (checkout) do store na etapie instalacji,
/// żeby były dostępne przy odinstalowaniu.
pub fn install_hooks(src_dir: &Path, dest_dir: &Path) -> Result<()> {
    let hooks_src = src_dir.join("hooks");
    if !hooks_src.exists() { return Ok(()); }

    let hooks_dst = dest_dir.join("hooks");
    fs::create_dir_all(&hooks_dst).into_diagnostic()?;

    for kind in &[
        HookKind::PreInstall, HookKind::PostInstall,
        HookKind::PreRemove,  HookKind::PostRemove,
        HookKind::PostUpdate,
    ] {
        let src = hooks_src.join(kind.filename());
        if src.exists() {
            let dst = hooks_dst.join(kind.filename());
            fs::copy(&src, &dst).into_diagnostic()?;
            crate::utils::make_executable(&dst)?;
        }
    }

    Ok(())
}

/// Waliduj hooki podczas `hpm build`.
pub fn validate_hooks(dir: &Path) -> Vec<String> {
    let hooks_dir = dir.join("hooks");
    if !hooks_dir.exists() { return Vec::new(); }

    let mut warnings = Vec::new();
    let valid_hooks = [
        "pre-install.sh", "post-install.sh",
        "pre-remove.sh",  "post-remove.sh",
        "post-update.sh",
    ];

    if let Ok(rd) = fs::read_dir(&hooks_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !valid_hooks.contains(&name.as_str()) {
                warnings.push(format!("Unknown hook file: hooks/{} (ignored)", name));
            } else {
                // Sprawdź czy zaczyna się od shebang
                if let Ok(content) = fs::read(entry.path()) {
                    if !content.starts_with(b"#!") {
                        warnings.push(format!("hooks/{} has no shebang line", name));
                    }
                }
                // Sprawdź executable bit
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = entry.path().metadata() {
                    if meta.permissions().mode() & 0o111 == 0 {
                        warnings.push(format!(
                            "hooks/{} is not executable — fix: git update-index --chmod=+x hooks/{}",
                            name, name
                        ));
                    }
                }
            }
        }
    }

    warnings
}
