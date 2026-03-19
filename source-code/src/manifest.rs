use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildInfo {
    pub commands: Vec<String>,
    pub deb_deps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub deb_deps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub install_commands: Vec<String>,
    #[serde(default)]
    pub build: BuildInfo,
    #[serde(default)]
    pub runtime: RuntimeInfo,
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

impl Manifest {
    pub fn load_from_path(path: &str) -> Result<Self> {
        let info_path = format!("{}/info.hk", path);
        let mut config = hk_parser::load_hk_file(&info_path)
        .map_err(|e| anyhow!("Failed to load info.hk: {}", e))?;
        hk_parser::resolve_interpolations(&mut config)
        .map_err(|e| anyhow!("Failed to resolve interpolations: {}", e))?;

        let metadata = config
        .get("metadata")
        .ok_or(anyhow!("Missing [metadata] section"))?
        .as_map()
        .map_err(|_| anyhow!("Invalid metadata"))?;

        let name = metadata
        .get("name")
        .ok_or(anyhow!("Missing name"))?
        .as_string()
        .map_err(|_| anyhow!("Invalid name"))?;

        let version = metadata
        .get("version")
        .ok_or(anyhow!("Missing version"))?
        .as_string()
        .map_err(|_| anyhow!("Invalid version"))?;

        let authors = metadata
        .get("authors")
        .ok_or(anyhow!("Missing authors"))?
        .as_string()
        .map_err(|_| anyhow!("Invalid authors"))?;

        let license = metadata
        .get("license")
        .ok_or(anyhow!("Missing license"))?
        .as_string()
        .map_err(|_| anyhow!("Invalid license"))?;

        let description = config.get("description").and_then(|v| v.as_map().ok());
        let summary = description
        .and_then(|d| d.get("summary"))
        .and_then(|v| v.as_string().ok())
        .unwrap_or_default();
        let long = description
        .and_then(|d| d.get("long"))
        .and_then(|v| v.as_string().ok())
        .unwrap_or_default();

        let specs = config.get("specs").and_then(|v| v.as_map().ok());
        let mut system_specs = IndexMap::new();
        if let Some(s) = specs {
            for (k, v) in s {
                if k != "dependencies" {
                    system_specs.insert(k.clone(), v.as_string().map_err(|_| anyhow!("Invalid spec value"))?);
                }
            }
        }

        let deps = if let Some(d) = specs
        .and_then(|s| s.get("dependencies"))
        .and_then(|v| v.as_map().ok())
        {
            let mut m = IndexMap::new();
            for (k, v) in d {
                m.insert(k.clone(), v.as_string().map_err(|_| anyhow!("Invalid dep value"))?);
            }
            m
        } else {
            IndexMap::new()
        };

        let bins_map = metadata.get("bins").and_then(|v| v.as_map().ok());
        let mut bins = Vec::new();
        if let Some(bm) = bins_map {
            for (k, v) in bm {
                if v.as_string().map_err(|_| anyhow!("Invalid bin value"))? == "" {
                    bins.push(k.clone());
                }
            }
        }

        let sandbox_sec = config
        .get("sandbox")
        .ok_or(anyhow!("Missing [sandbox] section"))?
        .as_map()
        .map_err(|_| anyhow!("Invalid sandbox"))?;
        let network = sandbox_sec
        .get("network")
        .and_then(|v| v.as_bool().ok())
        .unwrap_or(false);
        let gui = sandbox_sec.get("gui").and_then(|v| v.as_bool().ok()).unwrap_or(false);
        let dev = sandbox_sec.get("dev").and_then(|v| v.as_bool().ok()).unwrap_or(false);
        let full_gui = sandbox_sec.get("full_gui").and_then(|v| v.as_bool().ok()).unwrap_or(false);
        let fs_map = sandbox_sec.get("filesystem").and_then(|v| v.as_map().ok());
        let mut filesystem = Vec::new();
        if let Some(fm) = fs_map {
            for (k, v) in fm {
                if v.as_string().map_err(|_| anyhow!("Invalid fs value"))? == "" {
                    filesystem.push(k.clone());
                }
            }
        }

        let install_sec = config.get("install").and_then(|v| v.as_map().ok());
        let mut install_commands = Vec::new();
        if let Some(is) = install_sec {
            if let Some(cmds) = is.get("commands").and_then(|v| v.as_map().ok()) {
                for (k, v) in cmds {
                    if v.as_string().map_err(|_| anyhow!("Invalid cmd value"))? == "" {
                        install_commands.push(k.clone());
                    }
                }
            }
        }

        // Build section
        let build_sec = config.get("build").and_then(|v| v.as_map().ok());
        let mut build_commands = Vec::new();
        let mut build_deb_deps = Vec::new();
        if let Some(b) = build_sec {
            if let Some(cmds) = b.get("commands").and_then(|v| v.as_map().ok()) {
                for (k, v) in cmds {
                    if v.as_string().map_err(|_| anyhow!("Invalid cmd value"))? == "" {
                        build_commands.push(k.clone());
                    }
                }
            }
            if let Some(deps) = b.get("deb_deps").and_then(|v| v.as_map().ok()) {
                for (k, v) in deps {
                    if v.as_string().map_err(|_| anyhow!("Invalid dep value"))? == "" {
                        build_deb_deps.push(k.clone());
                    }
                }
            }
        }

        // Runtime section
        let runtime_sec = config.get("runtime").and_then(|v| v.as_map().ok());
        let mut runtime_deb_deps = Vec::new();
        if let Some(r) = runtime_sec {
            if let Some(deps) = r.get("deb_deps").and_then(|v| v.as_map().ok()) {
                for (k, v) in deps {
                    if v.as_string().map_err(|_| anyhow!("Invalid dep value"))? == "" {
                        runtime_deb_deps.push(k.clone());
                    }
                }
            }
        }

        Ok(Manifest {
            name,
            version,
            authors,
            license,
            summary,
            long,
            system_specs,
            deps,
            bins,
            sandbox: Sandbox {
                network,
                filesystem,
                gui,
                dev,
                full_gui,
            },
            install_commands,
            build: BuildInfo {
                commands: build_commands,
                deb_deps: build_deb_deps,
            },
            runtime: RuntimeInfo {
                deb_deps: runtime_deb_deps,
            },
        })
    }
}
