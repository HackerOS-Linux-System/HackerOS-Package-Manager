use miette::Diagnostic;
use thiserror::Error;

#[derive(Error, Diagnostic, Debug)]
pub enum HpmError {
    #[error("Package not found: {0}")]
    #[diagnostic(code(hpm::package_not_found))]
    PackageNotFound(String),

    #[error("Version not found: {0}@{1}")]
    #[diagnostic(code(hpm::version_not_found))]
    VersionNotFound(String, String),

    #[error("IO error: {0}")]
    #[diagnostic(code(hpm::io_error))]
    Io(#[from] std::io::Error),

    #[error("Git error: {0}")]
    #[diagnostic(code(hpm::git_error))]
    Git(#[from] git2::Error),

    #[error("Network error: {0}")]
    #[diagnostic(code(hpm::network_error))]
    Network(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    #[diagnostic(code(hpm::json_error))]
    Json(#[from] serde_json::Error),

    #[error("Invalid package manifest: {0}")]
    #[diagnostic(code(hpm::manifest_error))]
    Manifest(String),

    #[error("Permission denied: {0}")]
    #[diagnostic(code(hpm::permission_denied))]
    Permission(String),

    #[error("Sandbox error: {0}")]
    #[diagnostic(code(hpm::sandbox_error))]
    Sandbox(String),

    #[error("Invalid argument: {0}")]
    #[diagnostic(code(hpm::invalid_arg))]
    InvalidArg(String),
}
