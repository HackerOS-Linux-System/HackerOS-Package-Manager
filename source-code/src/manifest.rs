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

/// Desktop integration metadata (for GUI apps).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesktopInfo {
    /// Display name shown in app menu (falls back to name).
    pub display_name: String,
    /// Icon name or path inside contents/ (e.g. "icons/myapp.png").
    pub icon: String,
    /// XDG categories (e.g. "Graphics;Viewer").
    pub categories: String,
    /// Comment shown in .desktop file.
    pub comment: String,
    /// Whether to show in application menu.
    pub nodisplay: bool,
    /// Custom .desktop file path inside contents/ (overrides auto-generation).
    pub desktop_file: String,
    /// MIME types handled by this app.
    pub mime_types: String,
    /// Keywords for search.
    pub keywords: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub authors: String,
    pub license: String,
    pub summary: String,
    #[serde(default)]
    pub long: String,
    #[serde(default)]
    pub system_specs: IndexMap<String, String>,
    #[serde(default)]
    pub deps: IndexMap<String, String>,
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub sandbox: Sandbox,
    /// If true, run the binary directly with no sandbox at all.
    #[serde(default)]
    pub sandbox_disabled: bool,
    #[serde(default)]
    pub install_commands: Vec<String>,
    #[serde(default)]
    pub build: BuildInfo,
    #[serde(default)]
    pub runtime: RuntimeInfo,
    #[serde(default)]
    pub desktop: DesktopInfo,
    /// Whether this is a GUI application (shorthand that sets sandbox.gui=true).
    #[serde(default)]
    pub is_gui: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sandbox {
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub gui: bool,
    #[serde(default)]
    pub dev: bool,
    #[serde(default)]
    pub full_gui: bool,
}

fn is_empty_value(v: &HkValue) -> bool {
    match v {
        HkValue::String(s) => s.is_empty(),
        HkValue::Bool(b) => !b,
        _ => false,
    }
}

fn get_str(map: &IndexMap<String, HkValue>, key: &str) -> Option<String> {
    map.get(key)?.as_string().ok()
}

fn get_bool(map: &IndexMap<String, HkValue>, key: &str) -> bool {
    map.get(key).and_then(|v| v.as_bool().ok()).unwrap_or(false)
}

impl Manifest {
    pub fn load_from_path(path: &str) -> Result<Self> {
        let info_path = format!("{}/info.hk", path);
        let mut config = hk_parser::load_hk_file(&info_path)
        .map_err(|e| miette!("Failed to load info.hk: {}", e))?;
        hk_parser::resolve_interpolations(&mut config)
        .map_err(|e| miette!("Failed to resolve interpolations: {}", e))?;

        // ── [metadata] ──────────────────────────────────────────────────────
        let metadata = config.get("metadata")
        .ok_or_else(|| miette!("Missing [metadata] section"))?
        .as_map().map_err(|_| miette!("Invalid metadata"))?;

        let name    = get_str(metadata, "name").ok_or_else(|| miette!("Missing name"))?;
        let version = get_str(metadata, "version").ok_or_else(|| miette!("Missing version"))?;
        let authors = get_str(metadata, "authors").unwrap_or_default();
        let license = get_str(metadata, "license").unwrap_or_default();

        // bins: map keys where value is empty
        let bins_map = metadata.get("bins").and_then(|v| v.as_map().ok());
        let mut bins = Vec::new();
        if let Some(bm) = bins_map {
            for (k, v) in bm {
                if is_empty_value(v) { bins.push(k.clone()); }
            }
        }

        // is_gui shorthand
        let is_gui = get_bool(metadata, "gui");

        // ── [description] ───────────────────────────────────────────────────
        let description = config.get("description").and_then(|v| v.as_map().ok());
        let summary = description.and_then(|d| get_str(d, "summary")).unwrap_or_default();
        let long    = description.and_then(|d| get_str(d, "long")).unwrap_or_default();

        // ── [specs] ─────────────────────────────────────────────────────────
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

        // ── [sandbox] ───────────────────────────────────────────────────────
        let sandbox_sec = config.get("sandbox").and_then(|v| v.as_map().ok());
        let (network, gui, dev, full_gui, filesystem, sandbox_disabled) = if let Some(s) = sandbox_sec {
            let disabled = get_bool(s, "disabled");
            let fs_map = s.get("filesystem").and_then(|v| v.as_map().ok());
            let mut filesystem = Vec::new();
            if let Some(fm) = fs_map {
                for (k, v) in fm { if is_empty_value(v) { filesystem.push(k.clone()); } }
            }
            (get_bool(s, "network"), get_bool(s, "gui") || is_gui, get_bool(s, "dev"),
             get_bool(s, "full_gui"), filesystem, disabled)
        } else { (false, is_gui, false, false, Vec::new(), false) };

        // ── [install] ───────────────────────────────────────────────────────
        let install_sec = config.get("install").and_then(|v| v.as_map().ok());
        let mut install_commands = Vec::new();
        if let Some(is) = install_sec {
            if let Some(cmds) = is.get("commands").and_then(|v| v.as_map().ok()) {
                for (k, v) in cmds { if is_empty_value(v) { install_commands.push(k.clone()); } }
            }
        }

        // ── [build] ─────────────────────────────────────────────────────────
        let build_sec = config.get("build").and_then(|v| v.as_map().ok());
        let mut build_commands = Vec::new();
        let mut build_deb_deps = Vec::new();
        if let Some(b) = build_sec {
            if let Some(cmds) = b.get("commands").and_then(|v| v.as_map().ok()) {
                for (k, v) in cmds { if is_empty_value(v) { build_commands.push(k.clone()); } }
            }
            if let Some(deps) = b.get("deb_deps").and_then(|v| v.as_map().ok()) {
                for (k, v) in deps { if is_empty_value(v) { build_deb_deps.push(k.clone()); } }
            }
        }

        // ── [runtime] ───────────────────────────────────────────────────────
        let runtime_sec = config.get("runtime").and_then(|v| v.as_map().ok());
        let mut runtime_deb_deps = Vec::new();
        if let Some(r) = runtime_sec {
            if let Some(deps) = r.get("deb_deps").and_then(|v| v.as_map().ok()) {
                for (k, v) in deps { if is_empty_value(v) { runtime_deb_deps.push(k.clone()); } }
            }
        }

        // ── [desktop] ───────────────────────────────────────────────────────
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

        Ok(Manifest {
            name, version, authors, license, summary, long,
            system_specs, deps, bins, is_gui,
            sandbox: Sandbox { network, filesystem, gui, dev, full_gui },
            sandbox_disabled,
            install_commands,
            build: BuildInfo { commands: build_commands, deb_deps: build_deb_deps },
            runtime: RuntimeInfo { deb_deps: runtime_deb_deps },
            desktop,
        })
    }
}
