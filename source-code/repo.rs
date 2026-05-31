use miette::{Result, bail, miette, IntoDiagnostic};
use git2::{Repository, Oid, FetchOptions, RemoteCallbacks, Cred, build::RepoBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::manifest::Manifest;
use indicatif::ProgressBar;

const REPO_JSON_URL: &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/HackerOS-Package-Manager/main/repo/repo.json";

/// TTL cache metadanych (1 godzina).
const CACHE_TTL_SECS: u64 = 3600;

/// Max równoległych żądań HTTP w search_lightweight.
const SEARCH_CONCURRENCY: usize = 20;

fn meta_cache_dir() -> PathBuf {
    PathBuf::from("/var/cache/hpm/meta")
}

fn repos_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hpm/repos")
}

// ---------------------------------------------------------------------------
// repo.json — płaska struktura: name → git URL
// ---------------------------------------------------------------------------

/// Plik repo.json: klucz = nazwa pakietu, wartość = URL repozytorium git.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    /// name → git URL (jedyne co jest w repo.json)
    pub packages: HashMap<String, String>,
}

impl RepoIndex {
    /// Zwróć URL dla pakietu.
    pub fn url_of(&self, name: &str) -> Option<&str> {
        self.packages.get(name).map(|s| s.as_str())
    }

    /// Lista wszystkich nazw pakietów.
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.packages.keys().map(|s| s.as_str()).collect();
        v.sort();
        v
    }

    /// Liczba pakietów.
    pub fn len(&self) -> usize { self.packages.len() }
}

// ---------------------------------------------------------------------------
// PackageMeta — pobierane z info.hk, nie z repo.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMeta {
    pub name:    String,
    pub version: String,  // najnowszy tag
    pub summary: String,
    pub authors: String,
    pub license: String,
    /// Tagi grupowe z info.hk [metadata] -> tags
    #[serde(default)]
    pub tags: Vec<String>,
    /// Lista wszystkich tagów git (wersji) dostępnych w repo
    #[serde(default)]
    pub available_versions: Vec<String>,
    /// Unix timestamp pobrania
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
// Lekkie typy dla build_index (używane przez deps, outdated)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RepoPackage {
    pub name:     String,
    pub versions: Vec<PackageVersion>,
}

#[derive(Debug, Clone)]
pub struct PackageVersion {
    pub version:  String,
    pub commit:   Oid,
    pub manifest: Manifest,
    pub deps:     HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// build.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BuildSource {
    Download {
        url: String,
        #[serde(default)] binary_path:      String,
        #[serde(default)] strip_components: u32,
    },
    Build {
        commands: Vec<String>,
        output:   String,
    },
    Prebuilt,
}

impl Default for BuildSource {
    fn default() -> Self { BuildSource::Prebuilt }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildConfig {
    #[serde(default)] pub name:         String,
    #[serde(flatten)] pub source:       BuildSource,
    #[serde(default)] pub build_deps:   Vec<String>,
    #[serde(default)] pub runtime_deps: Vec<String>,
    #[serde(default)] pub env:          HashMap<String, String>,
    #[serde(default)] pub install_path: String,
}

impl BuildConfig {
    pub fn load_from_dir(dir: &Path) -> Option<Self> {
        let path = dir.join("build.toml");
        if !path.exists() { return None; }
        let content = fs::read_to_string(&path).ok()?;
        match toml::from_str::<BuildConfig>(&content) {
            Ok(cfg) => Some(cfg),
            Err(e)  => { eprintln!("Warning: failed to parse build.toml: {}", e); None }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
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

/// Zamień github.com URL na raw.githubusercontent.com
fn raw_base_url(repo_url: &str) -> Option<String> {
    let url = repo_url.trim_end_matches('/').trim_end_matches(".git");
    if url.contains("github.com") {
        Some(url.replace("https://github.com/", "https://raw.githubusercontent.com/"))
    } else { None }
}

async fn fetch_raw_file(client: &reqwest::Client, repo_url: &str, filename: &str) -> Result<String> {
    let base = raw_base_url(repo_url)
        .ok_or_else(|| miette!("Only GitHub repos are supported for fast HTTP fetch"))?;
    for branch in &["main", "master", "HEAD"] {
        let url = format!("{}/{}/{}", base, branch, filename);
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return resp.text().await.into_diagnostic();
            }
        }
    }
    bail!("Could not fetch '{}' from {}", filename, repo_url)
}

// ---------------------------------------------------------------------------
// Metadata cache
// ---------------------------------------------------------------------------

fn cache_path_for(pkg_name: &str) -> PathBuf {
    meta_cache_dir().join(format!("{}.json", pkg_name))
}

fn load_cached_meta(pkg_name: &str) -> Option<PackageMeta> {
    let path = cache_path_for(pkg_name);
    if !path.exists() { return None; }
    serde_json::from_slice(&fs::read(&path).ok()?).ok()
}

/// Publiczne API — używane przez install.rs i list.rs
pub fn load_cached_meta_pub(pkg_name: &str) -> Option<PackageMeta> {
    load_cached_meta(pkg_name)
}

fn save_cached_meta(meta: &PackageMeta) {
    let dir = meta_cache_dir();
    if fs::create_dir_all(&dir).is_err() { return; }
    if let Ok(data) = serde_json::to_vec(meta) {
        let _ = fs::write(cache_path_for(&meta.name), data);
    }
}

pub fn invalidate_meta_cache() {
    let dir = meta_cache_dir();
    if !dir.exists() { return; }
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() { let _ = fs::remove_file(entry.path()); }
    }
}

// ---------------------------------------------------------------------------
// Parsowanie info.hk → PackageMeta
// ---------------------------------------------------------------------------

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
                    name:               name.to_string(),
                    version:            manifest.version,
                    summary:            manifest.summary,
                    authors:            manifest.authors,
                    license:            manifest.license,
                    tags:               manifest.tags,
                    available_versions: Vec::new(), // uzupełniane przez fetch_package_meta
                    fetched_at:         now,
                };
            }
        }
    }

    // Fallback — prosta ekstrakcja linii
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
    PackageMeta {
        name: name.to_string(), version, summary, authors, license,
        tags: Vec::new(), available_versions: Vec::new(), fetched_at: now,
    }
}

fn extract_hk_value(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{} =", key);
    if !line.starts_with(&prefix) { return None; }
    let rest = line[prefix.len()..].trim();
    if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        Some(rest[1..rest.len()-1].to_string())
    } else {
        Some(rest.to_string())
    }
}

// ---------------------------------------------------------------------------
// RepoManager
// ---------------------------------------------------------------------------

pub struct RepoManager {
    pub index:  RepoIndex,
    client:     reqwest::Client,
}

impl RepoManager {
    // ── Load ─────────────────────────────────────────────────────────────────

    pub async fn load() -> Result<Self> {
        let client = make_client()?;
        let pb = ProgressBar::new_spinner();
        pb.set_message("Downloading package index...");

        let resp = client.get(REPO_JSON_URL).send().await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("certificate") || msg.contains("SSL") {
                    miette!("TLS error: {}\nFix: sudo apt install ca-certificates && sudo update-ca-certificates", e)
                } else if e.is_timeout() {
                    miette!("Connection timed out.")
                } else {
                    miette!("Network error: {}", e)
                }
            })?;

        if !resp.status().is_success() {
            bail!("Failed to download package index: HTTP {}", resp.status());
        }

        let index: RepoIndex = resp.json().await.into_diagnostic()?;
        pb.finish_with_message(format!("Index loaded ({} packages)", index.len()));
        Ok(RepoManager { index, client })
    }

    pub fn load_sync() -> Result<Self> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap()
            .block_on(Self::load())
    }

    // ── Accessors ────────────────────────────────────────────────────────────

    pub fn get_package_url(&self, name: &str) -> Option<&str> {
        self.index.url_of(name)
    }

    // ── Tags — czytane z info.hk, nie z repo.json ─────────────────────────

    /// Zwróć pakiety z danym tagiem.
    /// Tag pochodzi z pola `tags` w info.hk każdego pakietu (cache).
    pub fn packages_for_tag(&self, tag: &str) -> Vec<String> {
        let tag_lower = tag.to_lowercase();
        let mut result = Vec::new();
        for name in self.index.names() {
            if let Some(meta) = load_cached_meta(name) {
                if meta.tags.iter().any(|t| t.to_lowercase() == tag_lower) {
                    result.push(name.to_string());
                }
            }
        }
        result.sort();
        result
    }

    /// Zwróć wszystkie tagi które pojawiają się w cache metadanych.
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags = std::collections::HashSet::new();
        for name in self.index.names() {
            if let Some(meta) = load_cached_meta(name) {
                for t in meta.tags { tags.insert(t.to_lowercase()); }
            }
        }
        let mut v: Vec<String> = tags.into_iter().collect();
        v.sort();
        v
    }

    // ── HTTP raw file fetches ────────────────────────────────────────────────

    pub async fn fetch_raw_info_hk(&self, repo_url: &str) -> Result<String> {
        fetch_raw_file(&self.client, repo_url, "info.hk").await
    }

    pub async fn fetch_raw_build_config(&self, repo_url: &str) -> Option<BuildConfig> {
        let text = fetch_raw_file(&self.client, repo_url, "build.toml").await.ok()?;
        toml::from_str(&text).ok()
    }

    // ── PackageMeta fetch ────────────────────────────────────────────────────

    /// Pobierz metadane pakietu (cache lub HTTP).
    /// Wersje i tagi — z info.hk. Lista wersji — z tagów git jeśli repo sklonowane.
    pub async fn fetch_package_meta(&self, name: &str) -> Result<PackageMeta> {
        // Cache hit
        if let Some(cached) = load_cached_meta(name) {
            if !cached.is_stale() { return Ok(cached); }
        }

        let repo_url = self.index.url_of(name)
            .ok_or_else(|| miette!("Package '{}' not found in index", name))?;

        let content = self.fetch_raw_info_hk(repo_url).await?;
        let mut meta = parse_meta_from_content(name, &content);

        // Uzupełnij available_versions z lokalnego repo (jeśli sklonowane)
        meta.available_versions = get_local_versions(name);

        save_cached_meta(&meta);
        Ok(meta)
    }

    /// Wyszukiwanie z throttlingiem (max SEARCH_CONCURRENCY równoległo).
    /// Wszystkie metadane pochodzą z info.hk — nie z repo.json.
    pub async fn search_lightweight(&self, query: &str) -> Result<Vec<PackageMeta>> {
        let query_lower = query.trim_start_matches('@').to_lowercase();
        let is_tag      = query.starts_with('@');

        // Zbuduj listę kandydatów
        let candidates: Vec<(String, String)> = if query_lower.is_empty() {
            // Puste zapytanie = wszystkie pakiety (dla refresh)
            self.index.packages.iter()
                .map(|(n, u)| (n.clone(), u.clone()))
                .collect()
        } else {
            self.index.packages.iter()
                .filter(|(n, _)| {
                    // Przy wyszukiwaniu po tagu — sprawdź cache; przy nazwie — filtruj po nazwie
                    if is_tag {
                        if let Some(cached) = load_cached_meta(n) {
                            cached.tags.iter().any(|t| t.to_lowercase() == query_lower)
                        } else {
                            true // dołącz jeśli brak cache — sprawdzimy po pobraniu
                        }
                    } else {
                        n.to_lowercase().contains(&query_lower)
                    }
                })
                .map(|(n, u)| (n.clone(), u.clone()))
                .collect()
        };

        let mut all: Vec<PackageMeta> = Vec::new();

        for chunk in candidates.chunks(SEARCH_CONCURRENCY) {
            let futures: Vec<_> = chunk.iter().map(|(name, repo_url)| {
                let client    = self.client.clone();
                let ql        = query_lower.clone();
                let is_tag    = is_tag;
                let name      = name.clone();
                let repo_url  = repo_url.clone();

                async move {
                    // Cache hit
                    if let Some(cached) = load_cached_meta(&name) {
                        if !cached.is_stale() {
                            let matches = ql.is_empty()
                                || (!is_tag && (
                                    cached.name.to_lowercase().contains(&ql)
                                    || cached.summary.to_lowercase().contains(&ql)))
                                || (is_tag && cached.tags.iter().any(|t| t.to_lowercase() == ql));
                            return if matches { Some(cached) } else { None };
                        }
                    }
                    // Network fetch
                    match fetch_raw_file(&client, &repo_url, "info.hk").await {
                        Ok(content) => {
                            let mut meta = parse_meta_from_content(&name, &content);
                            meta.available_versions = get_local_versions(&name);
                            save_cached_meta(&meta);
                            let matches = ql.is_empty()
                                || (!is_tag && (
                                    meta.name.to_lowercase().contains(&ql)
                                    || meta.summary.to_lowercase().contains(&ql)))
                                || (is_tag && meta.tags.iter().any(|t| t.to_lowercase() == ql));
                            if matches { Some(meta) } else { None }
                        }
                        Err(_) => {
                            // Nie udało się pobrać — pokaż z nazwą jeśli pasuje
                            if ql.is_empty() || (!is_tag && name.to_lowercase().contains(&ql)) {
                                Some(PackageMeta {
                                    name: name.clone(),
                                    version: "unknown".to_string(),
                                    summary: "Could not fetch description".to_string(),
                                    authors: String::new(), license: String::new(),
                                    tags: Vec::new(), available_versions: Vec::new(),
                                    fetched_at: 0,
                                })
                            } else { None }
                        }
                    }
                }
            }).collect();

            let results: Vec<Option<PackageMeta>> = futures::future::join_all(futures).await;
            all.extend(results.into_iter().flatten());
        }

        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    // ── Git operations ────────────────────────────────────────────────────────

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
        let mut cb = RemoteCallbacks::new();
        cb.credentials(|url, _, _| {
            if url.starts_with("https://") { Cred::userpass_plaintext("", "") }
            else { Cred::ssh_key_from_agent("git") }
        });
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cb);
        let mut builder = RepoBuilder::new();
        builder.fetch_options(fo);
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
        let mut cb = RemoteCallbacks::new();
        cb.credentials(|url, _, _| {
            if url.starts_with("https://") { Cred::userpass_plaintext("", "") }
            else { Cred::ssh_key_from_agent("git") }
        });
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(cb);
        fo.download_tags(git2::AutotagOption::All);
        remote.fetch(
            &["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"],
            Some(&mut fo), None,
        ).map_err(|e| miette!("Failed to fetch {}: {}", url, e))?;
        Ok(())
    }

    pub fn get_latest_version_manifest(&self, repo_path: &Path) -> Result<(String, Manifest)> {
        let repo = Repository::open(repo_path).into_diagnostic()?;
        let mut tag_versions = collect_tag_manifests(&repo)?;

        if !tag_versions.is_empty() {
            tag_versions.sort_by(|a, b| crate::utils::compare_versions(&a.0, &b.0));
            let (v, _, m) = tag_versions.last().unwrap();
            return Ok((v.clone(), m.clone()));
        }

        // Fallback: HEAD
        let head   = repo.head().into_diagnostic()?;
        let commit = head.peel_to_commit().into_diagnostic()?;
        let tree   = commit.tree().into_diagnostic()?;
        if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
            let blob    = repo.find_blob(entry.id()).into_diagnostic()?;
            let content = String::from_utf8(blob.content().to_vec()).into_diagnostic()?;
            let tmp     = tempfile::tempdir().into_diagnostic()?;
            fs::write(tmp.path().join("info.hk"), &content).into_diagnostic()?;
            let manifest = Manifest::load_from_path(tmp.path().to_str().unwrap())?;
            let version  = manifest.version.clone();
            return Ok((version, manifest));
        }
        bail!("No info.hk found in repository")
    }

    pub fn build_index(&self) -> Result<HashMap<String, RepoPackage>> {
        let repos = repos_dir();
        let mut index = HashMap::new();

        for (name, _url) in &self.index.packages {
            let repo_path = repos.join(name);
            if !repo_path.exists() { continue; }

            let repo = Repository::open(&repo_path).into_diagnostic()?;
            let mut tag_versions = collect_tag_manifests(&repo)?;

            if tag_versions.is_empty() {
                // HEAD fallback
                if let Ok(head) = repo.head() {
                    if let Ok(commit) = head.peel_to_commit() {
                        if let Ok(tree) = commit.tree() {
                            if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
                                if let Ok(blob) = repo.find_blob(entry.id()) {
                                    if let Ok(content) = String::from_utf8(blob.content().to_vec()) {
                                        if let Ok(tmp) = tempfile::tempdir() {
                                            let _ = fs::write(tmp.path().join("info.hk"), &content);
                                            if let Ok(m) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                                                let ver  = m.version.clone();
                                                let deps = m.deps.clone().into_iter().collect();
                                                tag_versions.push((ver, commit.id(), m, deps));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            tag_versions.sort_by(|a, b| crate::utils::compare_versions(&a.0, &b.0));
            let versions = tag_versions.into_iter().map(|(version, commit, manifest, deps)| {
                PackageVersion { version, commit, manifest, deps }
            }).collect();

            index.insert(name.clone(), RepoPackage { name: name.clone(), versions });
        }
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Zbierz wszystkie wersje (tagi git) z lokalnie sklonowanego repo.
fn get_local_versions(pkg_name: &str) -> Vec<String> {
    let repo_path = repos_dir().join(pkg_name);
    if !repo_path.exists() { return Vec::new(); }
    let repo = match Repository::open(&repo_path) {
        Ok(r)  => r,
        Err(_) => return Vec::new(),
    };
    let tags = match repo.tag_names(None) {
        Ok(t)  => t,
        Err(_) => return Vec::new(),
    };
    let mut vers: Vec<String> = tags.iter().flatten()
        .map(|t| t.trim_start_matches('v').to_string())
        .collect();
    vers.sort_by(|a, b| crate::utils::compare_versions(a, b));
    vers
}

/// Wczytaj manifesty ze wszystkich tagów git w repo.
type TagManifests = Vec<(String, Oid, Manifest, HashMap<String, String>)>;

fn collect_tag_manifests(repo: &Repository) -> Result<TagManifests> {
    let tags = repo.tag_names(None).into_diagnostic()?;
    let mut result = Vec::new();

    for tag_name in tags.iter().flatten() {
        let ver_str = tag_name.trim_start_matches('v');
        let obj = match repo.revparse_single(tag_name) {
            Ok(o)  => o,
            Err(_) => continue,
        };
        let commit = match obj.peel_to_commit() {
            Ok(c)  => c,
            Err(_) => continue,
        };
        let tree = match commit.tree() {
            Ok(t)  => t,
            Err(_) => continue,
        };
        let entry = match tree.get_path(Path::new("info.hk")) {
            Ok(e)  => e,
            Err(_) => continue,
        };
        let blob = match repo.find_blob(entry.id()) {
            Ok(b)  => b,
            Err(_) => continue,
        };
        let content = match String::from_utf8(blob.content().to_vec()) {
            Ok(c)  => c,
            Err(_) => continue,
        };
        let tmp = match tempfile::tempdir() {
            Ok(t)  => t,
            Err(_) => continue,
        };
        if fs::write(tmp.path().join("info.hk"), &content).is_err() { continue; }
        let manifest = match Manifest::load_from_path(tmp.path().to_str().unwrap()) {
            Ok(m)  => m,
            Err(_) => continue,
        };
        let deps = manifest.deps.clone().into_iter().collect();
        result.push((ver_str.to_string(), commit.id(), manifest, deps));
    }
    Ok(result)
}
