use miette::{Result, miette};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use hk_parser::HkValue;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildInfo {
    pub commands: Vec<String>,
    pub deb_deps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub deb_deps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopInfo {
    pub display_name: String,
    pub icon:         String,
    pub categories:   String,
    pub comment:      String,
    pub nodisplay:    bool,
    pub desktop_file: String,
    pub mime_types:   String,
    pub keywords:     String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub name:    String,
    pub version: String,
    pub authors: String,
    pub license: String,
    pub summary: String,
    #[serde(default)] pub long:         String,
    #[serde(default)] pub system_specs: IndexMap<String, String>,
    #[serde(default)] pub deps:         IndexMap<String, String>,

    /// Tagi grupowe: @development, @cli, @tools itp.
    #[serde(default)] pub tags: Vec<String>,

    #[serde(default)] pub bins:      Vec<String>,
    #[serde(default)] pub bin_paths: IndexMap<String, String>,

    #[serde(default)] pub sandbox:          Sandbox,
    #[serde(default)] pub sandbox_disabled: bool,
    #[serde(default)] pub install_commands: Vec<String>,
    #[serde(default)] pub build:            BuildInfo,
    #[serde(default)] pub runtime:          RuntimeInfo,
    #[serde(default)] pub desktop:          DesktopInfo,
    #[serde(default)] pub is_gui:           bool,
    #[serde(default)] pub conflicts:        Vec<String>,

    /// Zadeklarowana architektura (x86_64 | aarch64 | armhf | i386 | any).
    /// Egzekwowana podczas instalacji.
    #[serde(default)] pub arch: String,

    /// Czy pakiet ma hooki (pre/post install/remove).
    /// Wykrywane automatycznie przez install.rs na podstawie katalogu hooks/.
    #[serde(default)] pub has_hooks: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sandbox {
    #[serde(default)] pub network:    bool,
    #[serde(default)] pub filesystem: Vec<String>,
    #[serde(default)] pub gui:        bool,
    #[serde(default)] pub dev:        bool,
    #[serde(default)] pub full_gui:   bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_empty_value(v: &HkValue) -> bool {
    match v {
        HkValue::String(s) => s.is_empty(),
        HkValue::Bool(b)   => !b,
        _                  => false,
    }
}

fn get_str(map: &IndexMap<String, HkValue>, key: &str) -> Option<String> {
    map.get(key)?.as_string().ok()
}

fn get_bool(map: &IndexMap<String, HkValue>, key: &str) -> bool {
    map.get(key).and_then(|v| v.as_bool().ok()).unwrap_or(false)
}

/// Parsuj wartość jako Vec<String>.
/// Obsługuje format mapowy { item => "" } (używany przez hk_parser 0.3.x).
/// Jeśli hk_parser doda obsługę [] w przyszłości — ten kod będzie nadal działał
/// bo najpierw próbuje as_map(), a potem fallback przez string.
fn get_string_list(map: &IndexMap<String, HkValue>, key: &str) -> Vec<String> {
    let val = match map.get(key) {
        Some(v) => v,
        None    => return Vec::new(),
    };

    // Format 1: mapa kluczy { item => "" }
    if let Ok(m) = val.as_map() {
        // Pusta mapa = pusta lista (np. -> filesystem => {})
        if m.is_empty() { return Vec::new(); }
        return m.iter()
            .filter(|(_, v)| is_empty_value(v) || matches!(v, HkValue::String(_)))
            .map(|(k, _)| k.clone())
            .collect();
    }

    // Format 2: pojedynczy string (edge case, np. -> tags => "cli")
    if let Ok(s) = val.as_string() {
        if s.trim().is_empty() || s.trim() == "[]" || s.trim() == "{}" {
            return Vec::new();
        }
        return s.split(',')
            .map(|x| x.trim().trim_matches('"').to_string())
            .filter(|x| !x.is_empty())
            .collect();
    }

    Vec::new()
}

// ---------------------------------------------------------------------------
// Arch validation
// ---------------------------------------------------------------------------

const VALID_ARCHS: &[&str] = &["x86_64", "aarch64", "armhf", "i386", "any"];

/// Sprawdź czy deklarowana architektura pasuje do bieżącej.
/// Zwraca błąd jeśli nie pasuje i nie jest "any".
pub fn check_arch_compatibility(declared: &str) -> Result<()> {
    if declared.is_empty() || declared == "any" { return Ok(()); }

    let current = std::env::consts::ARCH;
    let matches = match declared {
        "x86_64"  => current == "x86_64",
        "aarch64" => current == "aarch64",
        "armhf"   => current == "arm",
        "i386"    => current == "x86",
        _         => false,
    };

    if !matches {
        return Err(miette!(
            "Architecture mismatch: package requires '{}' but running on '{}'.\n\
  If this is a cross-arch install, use {} to override.",
            declared, current, "hpm install --force-arch".yellow()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest::load_from_path
// ---------------------------------------------------------------------------

impl Manifest {
    pub fn load_from_path(path: &str) -> Result<Self> {
        let info_path = format!("{}/info.hk", path);
        let mut config = hk_parser::load_hk_file(&info_path)
            .map_err(|e| miette!("Failed to load info.hk: {}", e))?;
        hk_parser::resolve_interpolations(&mut config)
            .map_err(|e| miette!("Failed to resolve interpolations: {}", e))?;

        // ── [metadata] ───────────────────────────────────────────────────────
        let metadata = config.get("metadata")
            .ok_or_else(|| miette!("Missing [metadata] section"))?
            .as_map().map_err(|_| miette!("Invalid [metadata] section"))?;

        let name    = get_str(metadata, "name").ok_or_else(|| miette!("Missing name"))?;
        let version = get_str(metadata, "version").ok_or_else(|| miette!("Missing version"))?;
        let authors = get_str(metadata, "authors").unwrap_or_default();
        let license = get_str(metadata, "license").unwrap_or_default();
        let is_gui  = get_bool(metadata, "gui");
        let arch    = get_str(metadata, "arch").unwrap_or_default();

        // Tagi grupowe
        let tags = get_string_list(metadata, "tags");

        // bins: map gdzie klucze = nazwy binariów, wartości = ścieżki lub ""
        let bins_map = metadata.get("bins").and_then(|v| v.as_map().ok());
        let mut bins      = Vec::new();
        let mut bin_paths = IndexMap::new();
        if let Some(bm) = bins_map {
            for (k, v) in bm {
                bins.push(k.clone());
                if let Ok(path_val) = v.as_string() {
                    if !path_val.is_empty() {
                        bin_paths.insert(k.clone(), path_val);
                    }
                }
            }
        }

        // ── [description] ────────────────────────────────────────────────────
        let description = config.get("description").and_then(|v| v.as_map().ok());
        let summary = description.and_then(|d| get_str(d, "summary")).unwrap_or_default();
        let long    = description.and_then(|d| get_str(d, "long")).unwrap_or_default();

        // ── [specs] ──────────────────────────────────────────────────────────
        let specs = config.get("specs").and_then(|v| v.as_map().ok());
        let mut system_specs = IndexMap::new();
        if let Some(s) = specs {
            for (k, v) in s {
                if k != "dependencies" {
                    if let Ok(val) = v.as_string() { system_specs.insert(k.clone(), val); }
                }
            }
        }
        let deps = if let Some(d) = specs
            .and_then(|s| s.get("dependencies"))
            .and_then(|v| v.as_map().ok())
        {
            let mut m = IndexMap::new();
            for (k, v) in d {
                if let Ok(val) = v.as_string() { m.insert(k.clone(), val); }
            }
            m
        } else { IndexMap::new() };

        // arch z [specs] jeśli nie w [metadata]
        let arch = if arch.is_empty() {
            system_specs.get("arch").cloned().unwrap_or_default()
        } else { arch };

        // ── [conflicts] ──────────────────────────────────────────────────────
        let conflicts_sec = config.get("conflicts").and_then(|v| v.as_map().ok());
        let mut conflicts = Vec::new();
        if let Some(c) = conflicts_sec {
            for (k, v) in c {
                if is_empty_value(v) { conflicts.push(k.clone()); }
            }
        }

        // ── [sandbox] ────────────────────────────────────────────────────────
        let sandbox_sec = config.get("sandbox").and_then(|v| v.as_map().ok());
        let (network, gui, dev, full_gui, filesystem, sandbox_disabled) =
            if let Some(s) = sandbox_sec {
                let filesystem = get_string_list(s, "filesystem");
                (
                    get_bool(s, "network"),
                    get_bool(s, "gui") || is_gui,
                    get_bool(s, "dev"),
                    get_bool(s, "full_gui"),
                    filesystem,
                    get_bool(s, "disabled"),
                )
            } else {
                (false, is_gui, false, false, Vec::new(), false)
            };

        // ── [install] ────────────────────────────────────────────────────────
        let install_sec      = config.get("install").and_then(|v| v.as_map().ok());
        let install_commands = install_sec
            .map(|is| get_string_list(is, "commands"))
            .unwrap_or_default();

        // ── [build] ──────────────────────────────────────────────────────────
        let build_sec = config.get("build").and_then(|v| v.as_map().ok());
        let (build_commands, build_deb_deps) = if let Some(b) = build_sec {
            (get_string_list(b, "commands"), get_string_list(b, "deb_deps"))
        } else { (Vec::new(), Vec::new()) };

        // ── [runtime] ────────────────────────────────────────────────────────
        let runtime_sec      = config.get("runtime").and_then(|v| v.as_map().ok());
        let runtime_deb_deps = runtime_sec
            .map(|r| get_string_list(r, "deb_deps"))
            .unwrap_or_default();

        // ── [desktop] ────────────────────────────────────────────────────────
        let desktop_sec = config.get("desktop").and_then(|v| v.as_map().ok());
        let desktop = if let Some(d) = desktop_sec {
            DesktopInfo {
                display_name: get_str(d, "display_name").unwrap_or_default(),
                icon:         get_str(d, "icon").unwrap_or_default(),
                categories:   get_str(d, "categories").unwrap_or_default(),
                comment:      get_str(d, "comment").unwrap_or_default(),
                nodisplay:    get_bool(d, "nodisplay"),
                desktop_file: get_str(d, "desktop_file").unwrap_or_default(),
                mime_types:   get_str(d, "mime_types").unwrap_or_default(),
                keywords:     get_str(d, "keywords").unwrap_or_default(),
            }
        } else { DesktopInfo::default() };

        // Wykryj hooki — sprawdź czy katalog hooks/ istnieje w ścieżce
        let has_hooks = std::path::Path::new(path).join("hooks").exists();

        Ok(Manifest {
            name, version, authors, license, summary, long,
            system_specs, deps, tags, bins, bin_paths, is_gui, conflicts, arch, has_hooks,
            sandbox: Sandbox { network, filesystem, gui, dev, full_gui },
            sandbox_disabled,
            install_commands,
            build:   BuildInfo   { commands: build_commands, deb_deps: build_deb_deps },
            runtime: RuntimeInfo { deb_deps: runtime_deb_deps },
            desktop,
        })
    }
}
