use miette::{Result, bail, miette, IntoDiagnostic};
use git2::{Repository, Oid, FetchOptions, RemoteCallbacks, Cred, build::RepoBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::manifest::Manifest;
use indicatif::ProgressBar;
use dirs;

const REPO_JSON_URL: &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/HackerOS-Package-Manager/main/repo/repo.json";

/// Metadata cache TTL in seconds (1 hour).
const CACHE_TTL_SECS: u64 = 3600;

/// Directory for cached package metadata.
fn meta_cache_dir() -> PathBuf {
    PathBuf::from("/var/cache/hpm/meta")
}

fn repos_dir() -> PathBuf {
    dirs::cache_dir()
    .unwrap_or_else(|| PathBuf::from("/tmp"))
    .join("hpm/repos")
}

// ---------------------------------------------------------------------------
// repo.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub packages: HashMap<String, PackageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEntry {
    pub repo: String,
    #[serde(default)]
    pub versions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RepoPackage {
    pub name: String,
    pub versions: Vec<PackageVersion>,
}

#[derive(Debug, Clone)]
pub struct PackageVersion {
    pub version: String,
    pub commit: Oid,
    pub manifest: Manifest,
    pub deps: HashMap<String, String>,
}

/// Lightweight metadata (used for search and info preview).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMeta {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub authors: String,
    pub license: String,
    /// Unix timestamp when this was fetched.
    #[serde(default)]
    pub fetched_at: u64,
}

impl PackageMeta {
    fn is_stale(&self) -> bool {
        let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
        now.saturating_sub(self.fetched_at) > CACHE_TTL_SECS
    }
}

// ---------------------------------------------------------------------------
// build.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BuildSource {
    Download {
        url: String,
        #[serde(default)] binary_path: String,
        #[serde(default)] strip_components: u32,
    },
    Build {
        commands: Vec<String>,
        output: String,
    },
    Prebuilt,
}

impl Default for BuildSource {
    fn default() -> Self { BuildSource::Prebuilt }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildConfig {
    #[serde(default)] pub name: String,
    #[serde(flatten)] pub source: BuildSource,
    #[serde(default)] pub build_deps: Vec<String>,
    #[serde(default)] pub runtime_deps: Vec<String>,
    #[serde(default)] pub env: HashMap<String, String>,
    #[serde(default)] pub install_path: String,
}

impl BuildConfig {
    pub fn load_from_dir(dir: &Path) -> Option<Self> {
        let path = dir.join("build.toml");
        if !path.exists() { return None; }
        let content = fs::read_to_string(&path).ok()?;
        match toml::from_str::<BuildConfig>(&content) {
            Ok(cfg) => Some(cfg),
            Err(e) => { eprintln!("Warning: failed to parse build.toml: {}", e); None }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

fn make_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(30))
    .build()
    .map_err(|e| miette!(
        "Failed to build HTTP client: {}\n\
SSL hint: sudo apt install ca-certificates && sudo update-ca-certificates", e
    ))
}

// ---------------------------------------------------------------------------
// Metadata cache helpers
// ---------------------------------------------------------------------------

fn cache_path_for(pkg_name: &str) -> PathBuf {
    meta_cache_dir().join(format!("{}.json", pkg_name))
}

fn load_cached_meta(pkg_name: &str) -> Option<PackageMeta> {
    let path = cache_path_for(pkg_name);
    if !path.exists() { return None; }
    let data = fs::read(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn save_cached_meta(meta: &PackageMeta) {
    let dir = meta_cache_dir();
    if fs::create_dir_all(&dir).is_err() { return; }
    let path = dir.join(format!("{}.json", meta.name));
    if let Ok(data) = serde_json::to_vec(meta) {
        let _ = fs::write(path, data);
    }
}

/// Invalidate all metadata cache entries (called by `hpm refresh`).
pub fn invalidate_meta_cache() {
    let dir = meta_cache_dir();
    if !dir.exists() { return; }
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// RepoManager
// ---------------------------------------------------------------------------

pub struct RepoManager {
    pub index: RepoIndex,
    client: reqwest::Client,
}

impl RepoManager {
    pub async fn load() -> Result<Self> {
        let client = make_client()?;
        let pb = ProgressBar::new_spinner();
        pb.set_message("Downloading package index...");

        let response = client.get(REPO_JSON_URL).send().await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("certificate") || msg.contains("SSL") || msg.contains("tls") {
                miette!(
                    "TLS error: {}\nFix: sudo apt install ca-certificates && sudo update-ca-certificates",
                    e
                )
            } else if e.is_timeout() {
                miette!("Connection timed out. Check your internet connection.")
            } else {
                miette!("Network error: {}", e)
            }
        })?;

        if !response.status().is_success() {
            bail!("Failed to download package index: HTTP {}", response.status());
        }
        let index: RepoIndex = response.json().await.into_diagnostic()?;
        pb.finish_with_message(format!("Index loaded ({} packages)", index.packages.len()));
        Ok(RepoManager { index, client })
    }

    pub fn load_sync() -> Result<Self> {
        tokio::runtime::Builder::new_current_thread()
        .enable_all().build().unwrap()
        .block_on(Self::load())
    }

    pub fn get_package_url(&self, name: &str) -> Option<&str> {
        self.index.packages.get(name).map(|e| e.repo.as_str())
    }

    pub fn get_package_versions(&self, name: &str) -> Option<&Vec<String>> {
        self.index.packages.get(name).map(|e| &e.versions)
    }

    // ── Raw HTTP helpers ────────────────────────────────────────────────────

    fn raw_base_url(repo_url: &str) -> Option<String> {
        let url = repo_url.trim_end_matches('/').trim_end_matches(".git");
        if url.contains("github.com") {
            Some(url.replace("https://github.com/", "https://raw.githubusercontent.com/"))
        } else { None }
    }

    async fn fetch_raw_file(client: &reqwest::Client, repo_url: &str, filename: &str) -> Result<String> {
        let base = Self::raw_base_url(repo_url)
        .ok_or_else(|| miette!("Only GitHub repos supported for fast fetch"))?;
        for branch in &["main", "master", "HEAD"] {
            let url = format!("{}/{}/{}", base, branch, filename);
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    return resp.text().await.into_diagnostic();
                }
            }
        }
        bail!("Could not fetch '{}' from {}", filename, repo_url);
    }

    pub async fn fetch_raw_info_hk(&self, repo_url: &str) -> Result<String> {
        Self::fetch_raw_file(&self.client, repo_url, "info.hk").await
    }

    pub async fn fetch_raw_build_config(&self, repo_url: &str) -> Option<BuildConfig> {
        let text = Self::fetch_raw_file(&self.client, repo_url, "build.toml").await.ok()?;
        toml::from_str(&text).ok()
    }

    pub fn parse_meta_from_content(name: &str, content: &str) -> PackageMeta {
        let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

        if let Ok(tmp) = tempfile::tempdir() {
            let info_path = tmp.path().join("info.hk");
            if fs::write(&info_path, content).is_ok() {
                if let Ok(manifest) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                    return PackageMeta {
                        name: name.to_string(),
                        version: manifest.version,
                        summary: manifest.summary,
                        authors: manifest.authors,
                        license: manifest.license,
                        fetched_at: now,
                    };
                }
            }
        }
        // Fallback
        let mut version = String::from("unknown");
        let mut summary = String::from("No description available");
        let mut authors = String::new();
        let mut license = String::new();
        for line in content.lines() {
            let line = line.trim();
            if let Some(v) = extract_hk_value(line, "version") { version = v; }
            if let Some(v) = extract_hk_value(line, "summary") { summary = v; }
            if let Some(v) = extract_hk_value(line, "authors") { authors = v; }
            if let Some(v) = extract_hk_value(line, "license") { license = v; }
        }
        PackageMeta { name: name.to_string(), version, summary, authors, license, fetched_at: now }
    }

    /// Fetch metadata for a package, using local cache when fresh.
    pub async fn fetch_package_meta(&self, name: &str) -> Result<PackageMeta> {
        // Try cache first
        if let Some(cached) = load_cached_meta(name) {
            if !cached.is_stale() {
                return Ok(cached);
            }
        }
        // Cache miss or stale — fetch from network
        let entry = self.index.packages.get(name)
        .ok_or_else(|| miette!("Package '{}' not found in index", name))?;
        let content = self.fetch_raw_info_hk(&entry.repo).await?;
        let meta = Self::parse_meta_from_content(name, &content);
        save_cached_meta(&meta);
        Ok(meta)
    }

    /// Search packages. Uses cache for already-fetched packages.
    /// Empty query = all packages (used by refresh).
    pub async fn search_lightweight(&self, query: &str) -> Result<Vec<PackageMeta>> {
        let query_lower = query.to_lowercase();

        let candidates: Vec<(String, String)> = if query_lower.is_empty() {
            self.index.packages.iter().map(|(n, e)| (n.clone(), e.repo.clone())).collect()
        } else {
            self.index.packages.iter()
            .filter(|(n, _)| n.to_lowercase().contains(&query_lower))
            .map(|(n, e)| (n.clone(), e.repo.clone()))
            .collect()
        };

        let futures_vec: Vec<_> = candidates.into_iter().map(|(name, repo_url)| {
            let client = self.client.clone();
            let ql = query_lower.clone();
            async move {
                // Check cache first
                if let Some(cached) = load_cached_meta(&name) {
                    if !cached.is_stale() {
                        let matches = ql.is_empty()
                        || cached.name.to_lowercase().contains(&ql)
                        || cached.summary.to_lowercase().contains(&ql);
                        return if matches { Some(cached) } else { None };
                    }
                }
                // Fetch from network
                match Self::fetch_raw_file(&client, &repo_url, "info.hk").await {
                    Ok(content) => {
                        let meta = Self::parse_meta_from_content(&name, &content);
                        save_cached_meta(&meta);
                        let matches = ql.is_empty()
                        || meta.name.to_lowercase().contains(&ql)
                        || meta.summary.to_lowercase().contains(&ql);
                        if matches { Some(meta) } else { None }
                    }
                    Err(_) => {
                        if ql.is_empty() || name.to_lowercase().contains(&ql) {
                            Some(PackageMeta {
                                name: name.clone(),
                                 version: "unknown".to_string(),
                                 summary: "Could not fetch description".to_string(),
                                 authors: String::new(),
                                 license: String::new(),
                                 fetched_at: 0,
                            })
                        } else { None }
                    }
                }
            }
        }).collect();

        let results: Vec<Option<PackageMeta>> = futures::future::join_all(futures_vec).await;
        let mut metas: Vec<PackageMeta> = results.into_iter().flatten().collect();
        metas.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(metas)
    }

    // ── Git operations ──────────────────────────────────────────────────────

    pub fn clone_package_repo(&self, name: &str, url: &str) -> Result<PathBuf> {
        let repo_path = repos_dir().join(name);
        if repo_path.exists() {
            self.update_repo(&repo_path, url)?;
        } else {
            self.clone_repo(url, &repo_path)?;
        }
        Ok(repo_path)
    }

    fn clone_repo(&self, url: &str, path: &Path) -> Result<()> {
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|url, _, _| {
            if url.starts_with("https://") { Cred::userpass_plaintext("", "") }
                else { Cred::ssh_key_from_agent("git") }
        });
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
        let mut builder = RepoBuilder::new();
        builder.fetch_options(fetch_opts);
        builder.clone(url, path).map_err(|e| miette!("Failed to clone {}: {}", url, e))?;
        Ok(())
    }

    fn update_repo(&self, path: &Path, url: &str) -> Result<()> {
        let repo = Repository::open(path).into_diagnostic()?;
        let mut remote = repo.find_remote("origin").into_diagnostic()?;
        if remote.url().unwrap_or("") != url {
            repo.remote_delete("origin").into_diagnostic()?;
            repo.remote("origin", url).into_diagnostic()?;
            remote = repo.find_remote("origin").into_diagnostic()?;
        }
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|url, _, _| {
            if url.starts_with("https://") { Cred::userpass_plaintext("", "") }
                else { Cred::ssh_key_from_agent("git") }
        });
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
        fetch_opts.download_tags(git2::AutotagOption::All);
        remote.fetch(
            &["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"],
            Some(&mut fetch_opts), None,
        ).map_err(|e| miette!("Failed to fetch: {}", e))?;
        Ok(())
    }

    pub fn find_commit_for_version(&self, repo_path: &Path, version: &str) -> Result<Oid> {
        let repo = Repository::open(repo_path).into_diagnostic()?;
        for tag_name in repo.tag_names(None).into_diagnostic()?.iter().flatten() {
            let obj = repo.revparse_single(tag_name).into_diagnostic()?;
            let commit = obj.peel_to_commit().into_diagnostic()?;
            if tag_name.trim_start_matches('v') == version { return Ok(commit.id()); }
        }
        bail!("Version {} not found in repository tags", version);
    }

    pub fn get_latest_version_manifest(&self, repo_path: &Path) -> Result<(String, Manifest)> {
        let repo = Repository::open(repo_path).into_diagnostic()?;
        let tags = repo.tag_names(None).into_diagnostic()?;
        let mut tag_versions = Vec::new();
        for tag_name in tags.iter().flatten() {
            let ver_str = tag_name.trim_start_matches('v');
            let obj = repo.revparse_single(tag_name).into_diagnostic()?;
            let commit = obj.peel_to_commit().into_diagnostic()?;
            let tree = commit.tree().into_diagnostic()?;
            if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
                let blob = repo.find_blob(entry.id()).into_diagnostic()?;
                let content = String::from_utf8(blob.content().to_vec()).into_diagnostic()?;
                let tmp = tempfile::tempdir().into_diagnostic()?;
                fs::write(tmp.path().join("info.hk"), &content).into_diagnostic()?;
                if let Ok(manifest) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                    tag_versions.push((ver_str.to_string(), commit.id(), manifest));
                }
            }
        }
        if !tag_versions.is_empty() {
            tag_versions.sort_by(|a, b| crate::utils::compare_versions(&a.0, &b.0));
            let (v, _, m) = tag_versions.last().unwrap();
            return Ok((v.clone(), m.clone()));
        }
        let head = repo.head().into_diagnostic()?;
        let commit = head.peel_to_commit().into_diagnostic()?;
        let tree = commit.tree().into_diagnostic()?;
        if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
            let blob = repo.find_blob(entry.id()).into_diagnostic()?;
            let content = String::from_utf8(blob.content().to_vec()).into_diagnostic()?;
            let tmp = tempfile::tempdir().into_diagnostic()?;
            fs::write(tmp.path().join("info.hk"), &content).into_diagnostic()?;
            let manifest = Manifest::load_from_path(tmp.path().to_str().unwrap())?;
            let version = manifest.version.clone();
            return Ok((version, manifest));
        }
        bail!("No info.hk found in repository");
    }

    pub async fn refresh(&self) -> Result<()> { Ok(()) }

    pub fn build_index(&self) -> Result<HashMap<String, RepoPackage>> {
        let repos_dir = repos_dir();
        let mut index = HashMap::new();
        for (name, _entry) in &self.index.packages {
            let repo_path = repos_dir.join(name);
            if !repo_path.exists() { continue; }
            let repo = Repository::open(&repo_path).into_diagnostic()?;
            let tags = repo.tag_names(None).into_diagnostic()?;
            let mut vers = Vec::new();
            for tag_name in tags.iter().flatten() {
                let obj = repo.revparse_single(tag_name).into_diagnostic()?;
                let commit = obj.peel_to_commit().into_diagnostic()?;
                let tree = commit.tree().into_diagnostic()?;
                if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
                    let blob = repo.find_blob(entry.id()).into_diagnostic()?;
                    let content = String::from_utf8(blob.content().to_vec()).into_diagnostic()?;
                    let tmp = tempfile::tempdir().into_diagnostic()?;
                    fs::write(tmp.path().join("info.hk"), &content).into_diagnostic()?;
                    if let Ok(manifest) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                        let version = manifest.version.clone();
                        let deps = manifest.deps.clone().into_iter().collect();
                        vers.push(PackageVersion { version, commit: commit.id(), manifest, deps });
                    }
                }
            }
            if vers.is_empty() {
                if let Ok(head) = repo.head() {
                    if let Ok(commit) = head.peel_to_commit() {
                        if let Ok(tree) = commit.tree() {
                            if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
                                if let Ok(blob) = repo.find_blob(entry.id()) {
                                    if let Ok(content) = String::from_utf8(blob.content().to_vec()) {
                                        if let Ok(tmp) = tempfile::tempdir() {
                                            let _ = fs::write(tmp.path().join("info.hk"), &content);
                                            if let Ok(manifest) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                                                let version = manifest.version.clone();
                                                let deps = manifest.deps.clone().into_iter().collect();
                                                vers.push(PackageVersion { version, commit: commit.id(), manifest, deps });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            vers.sort_by(|a, b| crate::utils::compare_versions(&a.version, &b.version));
            index.insert(name.clone(), RepoPackage { name: name.clone(), versions: vers });
        }
        Ok(index)
    }
}

fn extract_hk_value(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{} =", key);
    if !line.starts_with(&prefix) { return None; }
    let rest = line[prefix.len()..].trim();
    if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        Some(rest[1..rest.len() - 1].to_string())
    } else {
        Some(rest.to_string())
    }
}
