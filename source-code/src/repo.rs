use anyhow::{Context, Result, bail};
use git2::{Repository, Oid};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::manifest::Manifest;

const REPO_JSON_URL: &str = "https://raw.githubusercontent.com/HackerOS-Linux-System/Hacker-Package-Manager/main/repo/repo.json";
const REPOS_DIR: &str = "/var/cache/hpm/repos";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub packages: HashMap<String, PackageEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEntry {
    pub repo: String,
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

pub struct RepoManager {
    index: RepoIndex,
}

impl RepoManager {
    pub async fn load() -> Result<Self> {
        let client = reqwest::Client::new();
        let response = client.get(REPO_JSON_URL).send().await?;
        let index: RepoIndex = response.json().await?;
        Ok(RepoManager { index })
    }

    pub fn load_sync() -> Result<Self> {
        futures::executor::block_on(Self::load())
    }

    pub async fn refresh(&self) -> Result<()> {
        fs::create_dir_all(REPOS_DIR)?;
        for (name, _entry) in &self.index.packages {
            let repo_url = &self.index.packages[name].repo;
            let repo_path = Path::new(REPOS_DIR).join(name);
            if repo_path.exists() {
                let repo = Repository::open(&repo_path)?;
                let mut remote = repo.find_remote("origin")?;
                remote.fetch(&["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"], None, None)?;
            } else {
                Repository::clone(repo_url, &repo_path)?;
            }
        }
        Ok(())
    }

    pub fn build_index(&self) -> Result<HashMap<String, RepoPackage>> {
        let mut index = HashMap::new();
        for (name, _entry) in &self.index.packages {
            let repo_path = Path::new(REPOS_DIR).join(name);
            if !repo_path.exists() {
                continue;
            }
            let repo = Repository::open(&repo_path)?;
            let tags = repo.tag_names(None)?;
            let mut versions = Vec::new();
            for tag_name in tags.iter().flatten() {
                let obj = repo.revparse_single(tag_name)?;
                let commit = obj.peel_to_commit()?;
                let tree = commit.tree()?;
                if let Ok(entry) = tree.get_path(Path::new("info.hk")) {
                    let blob = repo.find_blob(entry.id())?;
                    let content = String::from_utf8(blob.content().to_vec())?;
                    let tmp_dir = tempfile::tempdir()?;
                    let info_path = tmp_dir.path().join("info.hk");
                    fs::write(&info_path, content)?;
                    let manifest = Manifest::load_from_path(tmp_dir.path().to_str().unwrap())?;
                    let version = manifest.version.clone();
                    let deps = manifest.deps.clone().into_iter().collect();
                    let pkg_version = PackageVersion {
                        version,
                        commit: commit.id(),
                        manifest: manifest.clone(),
                        deps,
                    };
                    versions.push(pkg_version);
                }
            }
            versions.sort_by(|a, b| crate::utils::compare_versions(&a.version, &b.version));
            index.insert(name.clone(), RepoPackage {
                name: name.clone(),
                         versions,
            });
        }
        Ok(index)
    }

    #[allow(deprecated)]
    pub fn checkout_package(&self, package: &str, version: &str, index: &HashMap<String, RepoPackage>) -> Result<PathBuf> {
        let pkg = index.get(package).context("Package not found")?;
        let ver = pkg.versions.iter().find(|v| v.version == version).context("Version not found")?;
        let repo_path = Path::new(REPOS_DIR).join(package);
        if !repo_path.exists() {
            bail!("Repository for package {} not found. Run refresh first.", package);
        }
        let repo = Repository::open(&repo_path)?;
        let commit = repo.find_commit(ver.commit)?;
        let tree = commit.tree()?;
        let checkout_dir = tempfile::tempdir()?;
        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.target_dir(checkout_dir.path());
        repo.checkout_tree(tree.as_object(), Some(&mut checkout_opts))?;
        Ok(checkout_dir.into_path())
    }
}
