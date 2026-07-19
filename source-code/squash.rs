use miette::{IntoDiagnostic, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn squashfs_sidecar(pkg_version_dir: &Path) -> Option<PathBuf> {
    let parent  = pkg_version_dir.parent()?;
    let version = pkg_version_dir.file_name()?.to_str()?;
    Some(parent.join(format!(".{}.squashfs", version)))
}

fn have_tool(name: &str) -> bool {
    Command::new("which").arg(name).output()
        .map(|o| o.status.success()).unwrap_or(false)
}

/// Czy `dir` jest AKTUALNIE zamontowanym punktem montowania (sprawdza
/// `/proc/mounts`, nie tylko "czy katalog istnieje i jest pusty" — pusty
/// katalog też jest prawidłowym, jeszcze-niezamontowanym stanem).
fn is_mount_point(dir: &Path) -> bool {
    let Ok(canon) = fs::canonicalize(dir) else { return false };
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else { return false };
    let target = canon.to_string_lossy();
    mounts.lines().any(|line| {
        line.split_whitespace().nth(1) == Some(target.as_ref())
    })
}

/// Kompresuje `pkg_version_dir` (świeżo zainstalowany, surowy katalog) do
/// pliku squashfs siostrzanego, po czym opróżnia oryginalny katalog (zostaje
/// jako czysty punkt montowania). Best-effort: jeśli `mksquashfs` nie jest
/// zainstalowane, po cichu zostawia pakiet nieskompresowany — brak
/// kompresji nigdy nie blokuje instalacji.
pub fn compress_after_install(pkg_version_dir: &Path) -> Result<bool> {
    if !have_tool("mksquashfs") {
        return Ok(false);
    }
    let Some(sidecar) = squashfs_sidecar(pkg_version_dir) else { return Ok(false) };

    let tmp_sidecar = sidecar.with_extension("squashfs.building");
    let _ = fs::remove_file(&tmp_sidecar);

    let status = Command::new("mksquashfs")
        .arg(pkg_version_dir)
        .arg(&tmp_sidecar)
        .args(["-comp", "lz4", "-Xhc", "-noappend", "-no-progress"])
        .output();

    let Ok(output) = status else { return Ok(false) };
    if !output.status.success() {
        let _ = fs::remove_file(&tmp_sidecar);
        return Ok(false); // best-effort — zainstalowany pakiet zostaje nieskompresowany
    }

    fs::rename(&tmp_sidecar, &sidecar).into_diagnostic()?;

    // Opróżnij oryginalny katalog — zostaje jako mountpoint. Robimy to przez
    // rename-out + mkdir świeżego pustego katalogu, żeby nie było momentu
    // gdzie katalog fizycznie nie istnieje (inne procesy mogłyby akurat
    // czytać `info.hk` w tym momencie).
    let tmp_old = pkg_version_dir.with_extension("old-uncompressed");
    let _ = fs::remove_dir_all(&tmp_old);
    fs::rename(pkg_version_dir, &tmp_old).into_diagnostic()?;
    fs::create_dir_all(pkg_version_dir).into_diagnostic()?;
    fs::remove_dir_all(&tmp_old).into_diagnostic()?;

    Ok(true)
}

/// Upewnia się że `pkg_version_dir` jest zamontowany, jeśli ma odpowiadający
/// mu plik `.squashfs`. Idempotentne i tanie do wołania za każdym razem
/// przed dostępem do store'owanego pakietu (`hpm run`, `remove`, `verify`,
/// `doctor`, hooki) — jeśli już zamontowane, tylko sprawdza `/proc/mounts` i
/// wraca; jeśli nie ma pliku `.squashfs` (stary/nieskompresowany pakiet),
/// nic nie robi.
pub fn ensure_mounted(pkg_version_dir: &Path) -> Result<()> {
    let Some(sidecar) = squashfs_sidecar(pkg_version_dir) else { return Ok(()) };
    if !sidecar.exists() {
        return Ok(()); // pakiet nieskompresowany (stary albo mksquashfs był niedostępny) — nic do zrobienia
    }
    if is_mount_point(pkg_version_dir) {
        return Ok(()); // już zamontowany, nic do roboty
    }
    if !have_tool("squashfuse") {
        // Mamy skompresowany obraz, ale nie ma czym go zamontować — to
        // faktyczny błąd (dane są "zamknięte" i niedostępne), nie cichy no-op.
        miette::bail!(
            "Package data is compressed (squashfs) but 'squashfuse' is not installed.\n  \
  Install it: apt install squashfuse (or equivalent for your distro)."
        );
    }
    fs::create_dir_all(pkg_version_dir).into_diagnostic()?;
    let output = Command::new("squashfuse")
        .arg(&sidecar)
        .arg(pkg_version_dir)
        .output()
        .into_diagnostic()?;
    if !output.status.success() {
        miette::bail!(
            "Failed to mount {}: {}",
            sidecar.display(), String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Odmontowuje (jeśli zamontowany) — wywoływane przed usunięciem wersji ze
/// store, żeby nie zostawić osieroconego uchwytu FUSE po skasowaniu plików
/// spod niego.
pub fn unmount(pkg_version_dir: &Path) -> Result<()> {
    if !is_mount_point(pkg_version_dir) {
        return Ok(());
    }
    // fusermount -u jest preferowane dla FUSE (nie wymaga roota); umount jako fallback.
    let ok = Command::new("fusermount").args(["-u"]).arg(pkg_version_dir)
        .status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        let _ = Command::new("umount").arg(pkg_version_dir).status();
    }
    Ok(())
}

/// Kasuje sam plik `.squashfs` (jeśli istnieje) — wołane RAZEM z usunięciem
/// katalogu wersji, inaczej zostałby osierocony (sam mountpoint jest pusty
/// po odmontowaniu, ale dane wciąż siedziałyby w pliku siostrzanym).
pub fn remove_sidecar(pkg_version_dir: &Path) -> Result<()> {
    if let Some(sidecar) = squashfs_sidecar(pkg_version_dir) {
        if sidecar.exists() {
            fs::remove_file(&sidecar).into_diagnostic()?;
        }
    }
    Ok(())
}

/// Czy ten katalog pakietu jest skompresowany (ma plik `.squashfs`
/// siostrzany) — używane do raportowania w `hpm info`/`hpm doctor`.
pub fn is_compressed(pkg_version_dir: &Path) -> bool {
    squashfs_sidecar(pkg_version_dir).map(|p| p.exists()).unwrap_or(false)
}

/// Rozmiar na dysku — dla pakietu skompresowanego to rozmiar pliku
/// `.squashfs`, nie (zwykle pusty/niezamontowany) katalog. Używane przez
/// `hpm clean --all` i `hpm list` do pokazania realnego zużycia miejsca.
pub fn on_disk_size(pkg_version_dir: &Path) -> u64 {
    if let Some(sidecar) = squashfs_sidecar(pkg_version_dir) {
        if let Ok(meta) = fs::metadata(&sidecar) {
            return meta.len();
        }
    }
    crate::commands::clean::dir_size(pkg_version_dir)
}
