use miette::{Result, bail, miette, IntoDiagnostic};
use git2::{Repository, Oid, FetchOptions, RemoteCallbacks, Cred, build::RepoBuilder};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::manifest::Manifest;
use indicatif::ProgressBar;

const REPO_JSON_URL: &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/HackerOS-Package-Manager/main/repo/repo.json";
const CACHE_TTL_SECS:    u64   = 3600;
const SEARCH_CONCURRENCY: usize = 20;

/// Dozwolone schematy URL repozytoriów git.
/// Blokuje file://, ftp://, i inne niebezpieczne schematy.
const ALLOWED_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git@"];

fn meta_cache_dir() -> PathBuf { PathBuf::from(format!("{}/meta", crate::cache_dir())) }

fn repos_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hpm/repos")
}

// ---------------------------------------------------------------------------
// Walidacja URL — NOWE (Krytyczne: blokuje file://, path traversal itp.)
// ---------------------------------------------------------------------------

pub fn validate_repo_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("Repository URL is empty");
    }

    // Sprawdź dozwolone schematy
    let allowed = ALLOWED_SCHEMES.iter().any(|s| url.starts_with(s));
    if !allowed {
        bail!(
            "Repository URL '{}' uses an unsupported scheme.\n\
  Only {} are allowed.\n\
  This prevents accidental access to local paths or unsafe protocols.",
            url,
            ALLOWED_SCHEMES.join(", ")
        );
    }

    // Dodatkowe sprawdzenie dla https:// — sprawdź że jest hostname
    if url.starts_with("https://") || url.starts_with("http://") {
        let without_scheme = url.trim_start_matches("https://").trim_start_matches("http://");
        let hostname = without_scheme.split('/').next().unwrap_or("");
        if hostname.is_empty() {
            bail!("Repository URL '{}' has no hostname", url);
        }
        // Blokuj IP localhost
        if hostname == "localhost" || hostname.starts_with("127.") || hostname.starts_with("::1") {
            bail!("Repository URL '{}' points to localhost — not allowed", url);
        }
    }

    // Blokuj path traversal w URL
    if url.contains("../") || url.contains("..\\") {
        bail!("Repository URL '{}' contains path traversal sequence", url);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// repo.json — płaska struktura
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub packages: HashMap<String, String>,
}

impl RepoIndex {
    pub fn url_of(&self, name: &str) -> Option<&str> {
        self.packages.get(name).map(|s| s.as_str())
    }
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.packages.keys().map(|s| s.as_str()).collect();
        v.sort();
        v
    }
    pub fn len(&self) -> usize { self.packages.len() }

    /// Waliduje wszystkie URL-e w indeksie (schemat, brak localhost/path
    /// traversal — patrz `validate_repo_url`). Używane zarówno przy
    /// pobieraniu prawdziwego repo.json, jak i przy ładowaniu lokalnego
    /// mirrora przez `HPM_REPO_JSON_URL=file://...`.
    pub fn validate_urls(&self) -> Result<()> {
        let mut bad_urls = Vec::new();
        for (name, url) in &self.packages {
            if let Err(e) = validate_repo_url(url) {
                bad_urls.push(format!("  {}: {}", name, e));
            }
        }
        if !bad_urls.is_empty() {
            bail!(
                "repo.json contains invalid repository URLs:\n{}\n\
  Contact the maintainer to fix repo.json.",
                bad_urls.join("\n")
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PackageMeta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMeta {
    pub name:               String,
    pub version:            String,
    pub summary:            String,
    pub authors:            String,
    pub license:            String,
    #[serde(default)] pub tags:               Vec<String>,
    #[serde(default)] pub available_versions: Vec<String>,
    #[serde(default)] pub fetched_at:         u64,
}

impl PackageMeta {
    fn is_stale(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0);
        now.saturating_sub(self.fetched_at) > CACHE_TTL_SECS
    }
}

// ---------------------------------------------------------------------------
// Build/index types
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BuildSource {
    Download {
        url: String,
        #[serde(default)] binary_path:      String,
        #[serde(default)] strip_components: u32,
    },
    Build { commands: Vec<String>, output: String },
    Prebuilt,
}

impl Default for BuildSource { fn default() -> Self { BuildSource::Prebuilt } }

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
        let content = fs::read_to_string(dir.join("build.toml")).ok()?;
        toml::from_str::<BuildConfig>(&content)
            .map_err(|e| eprintln!("Warning: failed to parse build.toml: {}", e))
            .ok()
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn make_client() -> Result<reqwest::Client> {
    // Obsługa proxy HTTP — reqwest czyta HTTP_PROXY/HTTPS_PROXY automatycznie
    // gdy zbudowany z feature "default-features = false" + "native-tls".
    // Dla explicit proxy użyj env var HTTPS_PROXY przed uruchomieniem hpm.
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| miette!(
            "Failed to build HTTP client: {}\n\
SSL hint: sudo apt install ca-certificates && sudo update-ca-certificates", e
        ))
}

fn raw_base_url(repo_url: &str) -> Option<String> {
    let url = repo_url.trim_end_matches('/').trim_end_matches(".git");
    if url.contains("github.com") {
        Some(url.replace("https://github.com/", "https://raw.githubusercontent.com/"))
    } else { None }
}

/// Nazwa katalogu cache dla klonów-jako-fallback metadanych (nie mylić z
/// prawdziwym cache instalacji, który używa nazwy pakietu) — pochodna z
/// samego URL-a, bo funkcje `fetch_raw_*` nie zawsze mają nazwę pakietu pod
/// ręką (tylko `repo_url`).
fn slug_for_url(url: &str) -> String {
    let base = url.trim_end_matches('/').trim_end_matches(".git");
    let last = base.rsplit('/').next().unwrap_or("repo");
    let slug: String = last.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if slug.is_empty() { "repo".to_string() } else { format!("_meta_{}", slug) }
}

async fn fetch_raw_file(client: &reqwest::Client, repo_url: &str, filename: &str) -> Result<String> {
    let base = raw_base_url(repo_url)
        .ok_or_else(|| miette!("Only GitHub repos supported for fast HTTP fetch"))?;
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

fn cache_path_for(name: &str) -> PathBuf { meta_cache_dir().join(format!("{}.json", name)) }

fn load_cached_meta(name: &str) -> Option<PackageMeta> {
    let data = fs::read(cache_path_for(name)).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn load_cached_meta_pub(name: &str) -> Option<PackageMeta> { load_cached_meta(name) }

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
        for e in rd.flatten() { let _ = fs::remove_file(e.path()); }
    }
}

// ---------------------------------------------------------------------------
// Parsowanie info.hk → PackageMeta
// ---------------------------------------------------------------------------

pub fn parse_meta_from_content(name: &str, content: &str) -> PackageMeta {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);

    if let Ok(tmp) = tempfile::tempdir() {
        if fs::write(tmp.path().join("info.hk"), content).is_ok() {
            if let Ok(m) = Manifest::load_from_path(tmp.path().to_str().unwrap()) {
                return PackageMeta {
                    name: name.to_string(), version: m.version, summary: m.summary,
                    authors: m.authors, license: m.license, tags: m.tags,
                    available_versions: Vec::new(), fetched_at: now,
                };
            }
        }
    }
    // Fallback text scan
    let mut version = "unknown".to_string();
    let mut summary = "No description available".to_string();
    let mut authors = String::new();
    let mut license = String::new();
    for line in content.lines() {
        let l = line.trim();
        if let Some(v) = extract_hk_value(l, "version") { version = v; }
        if let Some(v) = extract_hk_value(l, "summary") { summary = v; }
        if let Some(v) = extract_hk_value(l, "authors") { authors = v; }
        if let Some(v) = extract_hk_value(l, "license") { license = v; }
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
    } else { Some(rest.to_string()) }
}

// ---------------------------------------------------------------------------
// Wersje z lokalnego repo
// ---------------------------------------------------------------------------

fn get_local_versions(pkg_name: &str) -> Vec<String> {
    let repo_path = repos_dir().join(pkg_name);
    if !repo_path.exists() { return Vec::new(); }
    let repo = match Repository::open(&repo_path) { Ok(r) => r, Err(_) => return Vec::new() };
    let tags = match repo.tag_names(None) { Ok(t) => t, Err(_) => return Vec::new() };
    let mut vers: Vec<String> = tags.iter().flatten()
        .map(|t| t.trim_start_matches('v').to_string()).collect();
    vers.sort_by(|a, b| crate::utils::compare_versions(a, b));
    vers
}

// ---------------------------------------------------------------------------
// collect_tag_manifests — FIXED: zwraca 4-tuple (String, Oid, Manifest, HashMap)
// ---------------------------------------------------------------------------

type TagManifests = Vec<(String, Oid, Manifest, HashMap<String, String>)>;

fn collect_tag_manifests(repo: &Repository) -> Result<TagManifests> {
    let tags = repo.tag_names(None).into_diagnostic()?;
    let mut result = Vec::new();
    for tag_name in tags.iter().flatten() {
        let ver_str = tag_name.trim_start_matches('v');
        let obj    = match repo.revparse_single(tag_name) { Ok(o) => o, Err(_) => continue };
        let commit = match obj.peel_to_commit()            { Ok(c) => c, Err(_) => continue };
        let tree   = match commit.tree()                   { Ok(t) => t, Err(_) => continue };
        let entry  = match tree.get_path(Path::new("info.hk")) { Ok(e) => e, Err(_) => continue };
        let blob   = match repo.find_blob(entry.id())     { Ok(b) => b, Err(_) => continue };
        let content= match String::from_utf8(blob.content().to_vec()) { Ok(c) => c, Err(_) => continue };
        let tmp    = match tempfile::tempdir()             { Ok(t) => t, Err(_) => continue };
        if fs::write(tmp.path().join("info.hk"), &content).is_err() { continue; }
        let manifest = match Manifest::load_from_path(tmp.path().to_str().unwrap()) { Ok(m) => m, Err(_) => continue };
        let deps: HashMap<String, String> = manifest.deps.clone().into_iter().collect();
        result.push((ver_str.to_string(), commit.id(), manifest, deps));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// RepoManager
// ---------------------------------------------------------------------------

pub struct RepoManager {
    pub index:  RepoIndex,
    client:     reqwest::Client,
}

impl RepoManager {
    /// Zwraca URL indeksu pakietów. Domyślnie oficjalny `repo.json`, ale
    /// respektuje `HPM_REPO_JSON_URL` jeśli ustawione — przydatne dla
    /// self-hosted mirrorów firmowych, offline testów, i CI (podobnie jak
    /// `NPM_CONFIG_REGISTRY` / `PIP_INDEX_URL` w innych ekosystemach).
    fn repo_json_url() -> String {
        std::env::var("HPM_REPO_JSON_URL").unwrap_or_else(|_| REPO_JSON_URL.to_string())
    }

    pub async fn load() -> Result<Self> {
        let client = make_client()?;
        let pb = ProgressBar::new_spinner();
        pb.set_message("Downloading package index...");
        let url = Self::repo_json_url();

        // `file://` tylko dla HPM_REPO_JSON_URL (lokalny/self-hosted mirror
        // indeksu, NIE dla URL-i pakietów w repo.json samym — te nadal
        // przechodzą przez `validate_repo_url` i normalny `ALLOWED_SCHEMES`).
        if let Some(local_path) = url.strip_prefix("file://") {
            let data = std::fs::read_to_string(local_path)
                .map_err(|e| miette!("Failed to read local repo index '{}': {}", local_path, e))?;
            let index: RepoIndex = serde_json::from_str(&data).into_diagnostic()?;
            index.validate_urls()?;
            pb.finish_with_message(format!("Index loaded from {} ({} packages)", local_path, index.len()));
            return Ok(RepoManager { index, client });
        }

        let resp = client.get(&url).send().await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("certificate") || msg.contains("SSL") {
                    miette!("TLS error: {}\nFix: sudo apt install ca-certificates && sudo update-ca-certificates", e)
                } else if e.is_timeout() {
                    miette!("Connection timed out. Check your internet connection.")
                } else {
                    miette!("Network error: {}", e)
                }
            })?;
        if !resp.status().is_success() {
            bail!("Failed to download package index: HTTP {}", resp.status());
        }
        let index: RepoIndex = resp.json().await.into_diagnostic()?;

        // Waliduj wszystkie URL z repo.json przy ładowaniu
        index.validate_urls()?;

        pb.finish_with_message(format!("Index loaded ({} packages)", index.len()));
        Ok(RepoManager { index, client })
    }

    pub fn load_sync() -> Result<Self> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap()
            .block_on(Self::load())
    }

    /// Konstruktor z gotowego indeksu, bez pobierania repo.json z sieci —
    /// używane przez `hpm dev test-full` do testowania solvera wersji na
    /// lokalnych repo git, bez zależności od prawdziwej sieci.
    pub fn from_index(index: RepoIndex) -> Result<Self> {
        Ok(RepoManager { index, client: make_client()? })
    }

    pub fn get_package_url(&self, name: &str) -> Option<&str> { self.index.url_of(name) }

    // ── Tagi — z cache info.hk, nie z repo.json ──────────────────────────────

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

    // ── HTTP helpers ──────────────────────────────────────────────────────────

    pub async fn fetch_raw_info_hk(&self, repo_url: &str) -> Result<String> {
        match fetch_raw_file(&self.client, repo_url, "info.hk").await {
            Ok(text) => Ok(text),
            // BUG NAPRAWIONY (znaleziony testując `hpm info`/`hpm search` na
            // pakietach spoza GitHub): szybka ścieżka HTTP
            // (raw.githubusercontent.com) z definicji działa tylko dla
            // GitHub — dla każdego innego hosta po prostu się poddawała.
            // Teraz spada do (cache'owanego) `git clone` i czyta info.hk
            // lokalnie z czubka domyślnej gałęzi — wolniejsze niż HTTP, ale
            // rzeczywiście DZIAŁA zamiast twardo wymagać GitHub.
            Err(_) if raw_base_url(repo_url).is_none() => {
                self.fetch_info_hk_via_clone(repo_url)
            }
            Err(e) => Err(e),
        }
    }

    fn fetch_info_hk_via_clone(&self, repo_url: &str) -> Result<String> {
        let slug = slug_for_url(repo_url);
        let repo_path = self.clone_package_repo(&slug, repo_url)?;
        let repo   = Repository::open(&repo_path).into_diagnostic()?;
        let head   = repo.head().into_diagnostic()?;
        let commit = head.peel_to_commit().into_diagnostic()?;
        let tree   = commit.tree().into_diagnostic()?;
        let entry  = tree.get_path(Path::new("info.hk"))
            .map_err(|_| miette!("info.hk not found in {} (default branch)", repo_url))?;
        let blob = repo.find_blob(entry.id()).into_diagnostic()?;
        String::from_utf8(blob.content().to_vec()).into_diagnostic()
    }

    pub async fn fetch_raw_build_config(&self, repo_url: &str) -> Option<BuildConfig> {
        let text = match fetch_raw_file(&self.client, repo_url, "build.toml").await {
            Ok(t) => t,
            Err(_) if raw_base_url(repo_url).is_none() => {
                self.fetch_build_toml_via_clone(repo_url).ok()?
            }
            Err(_) => return None,
        };
        toml::from_str(&text).ok()
    }

    fn fetch_build_toml_via_clone(&self, repo_url: &str) -> Result<String> {
        let slug = slug_for_url(repo_url);
        let repo_path = self.clone_package_repo(&slug, repo_url)?;
        let repo   = Repository::open(&repo_path).into_diagnostic()?;
        let head   = repo.head().into_diagnostic()?;
        let commit = head.peel_to_commit().into_diagnostic()?;
        let tree   = commit.tree().into_diagnostic()?;
        let entry  = tree.get_path(Path::new("build.toml")).into_diagnostic()?;
        let blob = repo.find_blob(entry.id()).into_diagnostic()?;
        String::from_utf8(blob.content().to_vec()).into_diagnostic()
    }

    pub async fn fetch_package_meta(&self, name: &str) -> Result<PackageMeta> {
        if let Some(cached) = load_cached_meta(name) {
            if !cached.is_stale() { return Ok(cached); }
        }
        let repo_url = self.index.url_of(name)
            .ok_or_else(|| miette!("Package '{}' not found in index", name))?;
        let content  = self.fetch_raw_info_hk(repo_url).await?;
        let mut meta = parse_meta_from_content(name, &content);
        meta.available_versions = get_local_versions(name);
        save_cached_meta(&meta);
        Ok(meta)
    }

    /// Wyszukiwanie z throttlingiem.
    /// Tryb offline: gdy HTTP zawiedzie — używa cache.
    pub async fn search_lightweight(&self, query: &str) -> Result<Vec<PackageMeta>> {
        let is_tag      = query.starts_with('@');
        let query_lower = query.trim_start_matches('@').to_lowercase();

        let candidates: Vec<(String, String)> = if query_lower.is_empty() {
            self.index.packages.iter().map(|(n, u)| (n.clone(), u.clone())).collect()
        } else {
            self.index.packages.iter()
                .filter(|(n, _)| {
                    if is_tag {
                        // Sprawdź cache — jeśli jest i nie jest stale, filtruj po tagu
                        // Jeśli brak cache — dołącz (sprawdzimy po fetch)
                        load_cached_meta(n)
                            .map(|m| m.tags.iter().any(|t| t.to_lowercase() == query_lower))
                            .unwrap_or(true)
                    } else {
                        n.to_lowercase().contains(&query_lower)
                    }
                })
                .map(|(n, u)| (n.clone(), u.clone()))
                .collect()
        };

        let mut all: Vec<PackageMeta> = Vec::new();

        for chunk in candidates.chunks(SEARCH_CONCURRENCY) {
            let futs: Vec<_> = chunk.iter().map(|(name, repo_url)| {
                let client   = self.client.clone();
                let ql       = query_lower.clone();
                let is_tag   = is_tag;
                let name     = name.clone();
                let repo_url = repo_url.clone();

                async move {
                    // Cache hit — nawet stale cache jest OK dla offline mode
                    if let Some(cached) = load_cached_meta(&name) {
                        if !cached.is_stale() {
                            let matches = ql.is_empty()
                                || (!is_tag && (cached.name.to_lowercase().contains(&ql)
                                    || cached.summary.to_lowercase().contains(&ql)))
                                || (is_tag && cached.tags.iter().any(|t| t.to_lowercase() == ql));
                            return if matches { Some(cached) } else { None };
                        }
                    }

                    // Network fetch — z fallbackiem do stale cache przy błędzie sieci
                    let fetch_result = match fetch_raw_file(&client, &repo_url, "info.hk").await {
                        Ok(content) => Ok(content),
                        // Nie-GitHub: szybka ścieżka HTTP nie ma zastosowania —
                        // sklonuj (cache'owane) i przeczytaj lokalnie zamiast
                        // po prostu dawać "Could not fetch (offline?)".
                        Err(_) if raw_base_url(&repo_url).is_none() => {
                            let name2 = name.clone();
                            let url2  = repo_url.clone();
                            tokio::task::spawn_blocking(move || {
                                let idx = RepoIndex { packages: std::collections::HashMap::new() };
                                let rm  = RepoManager::from_index(idx)?;
                                rm.fetch_info_hk_via_clone(&url2)
                                    .map_err(|e| miette!("clone fallback failed for {}: {}", name2, e))
                            }).await.into_diagnostic().and_then(|r| r)
                        }
                        Err(e) => Err(e),
                    };

                    match fetch_result {
                        Ok(content) => {
                            let mut meta = parse_meta_from_content(&name, &content);
                            meta.available_versions = get_local_versions(&name);
                            save_cached_meta(&meta);
                            let matches = ql.is_empty()
                                || (!is_tag && (meta.name.to_lowercase().contains(&ql)
                                    || meta.summary.to_lowercase().contains(&ql)))
                                || (is_tag && meta.tags.iter().any(|t| t.to_lowercase() == ql));
                            if matches { Some(meta) } else { None }
                        }
                        // NOWE: tryb offline — przy błędzie sieci użyj stale cache
                        Err(_) => {
                            if let Some(stale) = load_cached_meta(&name) {
                                let matches = ql.is_empty()
                                    || (!is_tag && (stale.name.to_lowercase().contains(&ql)
                                        || stale.summary.to_lowercase().contains(&ql)))
                                    || (is_tag && stale.tags.iter().any(|t| t.to_lowercase() == ql));
                                return if matches { Some(stale) } else { None };
                            }
                            if ql.is_empty() || (!is_tag && name.to_lowercase().contains(&ql)) {
                                Some(PackageMeta {
                                    name: name.clone(), version: "unknown".to_string(),
                                    summary: "Could not fetch (offline?)".to_string(),
                                    authors: String::new(), license: String::new(),
                                    tags: Vec::new(), available_versions: get_local_versions(&name),
                                    fetched_at: 0,
                                })
                            } else { None }
                        }
                    }
                }
            }).collect();

            let results: Vec<Option<PackageMeta>> = futures::future::join_all(futs).await;
            all.extend(results.into_iter().flatten());
        }

        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    // ── Git operations ────────────────────────────────────────────────────────

    pub fn clone_package_repo(&self, name: &str, url: &str) -> Result<PathBuf> {
        // Waliduj URL jeszcze raz przed klonowaniem (defense in depth)
        validate_repo_url(url)?;
        let repo_path = repos_dir().join(name);
        if repo_path.exists() { self.update_repo(&repo_path, url)?; }
        else                  { self.clone_repo(url, &repo_path)?; }
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
        ).map_err(|e| miette!("Failed to fetch: {}", e))?;
        Ok(())
    }

    // FIXED: destrukturyzacja 4-tuple (v, _, m, _)
    pub fn get_latest_version_manifest(&self, repo_path: &Path) -> Result<(String, Manifest)> {
        let repo = Repository::open(repo_path).into_diagnostic()?;
        let mut tag_versions = collect_tag_manifests(&repo)?;
        if !tag_versions.is_empty() {
            tag_versions.sort_by(|a, b| crate::utils::compare_versions(&a.0, &b.0));
            // FIXED: 4-tuple — destrukturyzujemy wszystkie 4 pola
            let (v, _, m, _) = tag_versions.last().unwrap();
            return Ok((v.clone(), m.clone()));
        }
        // HEAD fallback
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
            let versions = tag_versions.into_iter()
                .map(|(version, commit, manifest, deps)| PackageVersion { version, commit, manifest, deps })
                .collect();
            index.insert(name.clone(), RepoPackage { name: name.clone(), versions });
        }
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// GitHub Releases — `.hpm` jako alternatywa dla pełnego `git clone`
//
// repo.json / repo-list.json NIE zmienia formatu: pakiet nadal jest po prostu
// URL-em repozytorium git (`"name": "https://github.com/owner/repo"`). Kiedy
// użytkownik doda `--release` do `hpm install`, hpm bierze ten sam URL,
// wywnioskowuje z niego owner/repo, i pyta GitHub API o Release zamiast
// klonować całe repo — pod warunkiem że maintainer faktycznie coś tam
// wrzucił (`hpm build` + załącznik do Release). Jeśli nie ma Release albo
// assetu .hpm, zwracamy jasny błąd z podpowiedzią, nie fallbackujemy po cichu
// na git clone (żeby `--release` zawsze znaczyło dokładnie to, o co proszono).
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

/// Wyciąga (owner, repo) z URL-a github.com w dowolnym popularnym formacie:
/// https://github.com/owner/repo, https://github.com/owner/repo.git,
/// git@github.com:owner/repo.git.
pub fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let cleaned = url.trim().trim_end_matches(".git").trim_end_matches('/');
    let rest = if let Some(r) = cleaned.strip_prefix("https://github.com/") { r }
        else if let Some(r) = cleaned.strip_prefix("http://github.com/") { r }
        else if let Some(r) = cleaned.strip_prefix("git@github.com:") { r }
        else { return None };
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo  = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() { return None; }
    Some((owner, repo))
}

pub fn fetch_github_release(owner: &str, repo: &str, tag: Option<&str>) -> Result<GithubRelease> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all().build().into_diagnostic()?
        .block_on(fetch_github_release_async(owner, repo, tag))
}

async fn fetch_github_release_async(owner: &str, repo: &str, tag: Option<&str>) -> Result<GithubRelease> {
    let url = match tag {
        Some(t) => format!("https://api.github.com/repos/{}/{}/releases/tags/{}", owner, repo, t),
        None    => format!("https://api.github.com/repos/{}/{}/releases/latest", owner, repo),
    };
    let client = make_client()?;
    let resp = client.get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "hpm-package-manager")
        .send().await
        .map_err(|e| miette!("Failed to reach GitHub API: {}", e))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(miette!(
            "No {} found for {}/{} on GitHub.\n  \
  The maintainer needs to publish one (see `hpm build`), or drop --release to use git clone instead.",
            match tag { Some(t) => format!("release tagged '{}'", t), None => "releases".to_string() },
            owner, repo
        ));
    }
    if !resp.status().is_success() {
        bail!("GitHub API returned {} for {}/{}", resp.status(), owner, repo);
    }
    resp.json::<GithubRelease>().await.into_diagnostic()
}
