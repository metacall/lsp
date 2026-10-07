//! Boundary types: one parse at the protocol edge, no invalid state inside.
use std::fmt;
use std::path::{Path, PathBuf};

use url::Url;

use crate::error::ServerError;

/// One document address, parsed once at the boundary; `to_path` yields a path only for a file URI.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DocUri {
    url: Url,
    path: Option<PathBuf>,
}

impl DocUri {
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Path of a file URI. `None` for any other scheme.
    pub fn to_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }
}

impl fmt::Display for DocUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for DocUri {
    type Error = ServerError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let url = Url::parse(value).map_err(|error| {
            ServerError::InvalidParams(format!("document URI {value}: {error}"))
        })?;
        let path = url
            .to_file_path()
            .ok()
            .map(|path| dunce::simplified(&path).to_path_buf());
        Ok(Self { url, path })
    }
}

impl TryFrom<&lsp_types::Uri> for DocUri {
    type Error = ServerError;

    fn try_from(value: &lsp_types::Uri) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

/// Document version the client sent; the buffer store keeps it strictly increasing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocVersion(i32);

impl DocVersion {
    pub fn value(self) -> i32 {
        self.0
    }
}

impl From<DocVersion> for i32 {
    fn from(version: DocVersion) -> Self {
        version.0
    }
}

impl From<i32> for DocVersion {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

/// Canonical project root: an existing directory, in the spelling buffers and watch paths use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDir(PathBuf);

impl RootDir {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn contains(&self, path: &Path) -> bool {
        dunce::simplified(path).starts_with(&self.0)
    }
}

impl TryFrom<&Path> for RootDir {
    type Error = anyhow::Error;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        if !path.is_dir() {
            anyhow::bail!(
                "workspace folder is not an existing directory: {}",
                path.display()
            );
        }
        Ok(Self(dunce::simplified(path).to_path_buf()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for LogLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => anyhow::bail!("unknown log level {other:?}"),
        }
    }
}

impl TryFrom<&str> for LogLevel {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, anyhow::Error> {
        value.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_uri_yields_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.py");
        let uri = crate::convert::path_to_uri(&path).expect("uri");
        let doc = DocUri::try_from(uri.as_str()).expect("file uri");

        assert_eq!(
            doc.to_path()
                .map(|path| dunce::simplified(&path).to_path_buf()),
            Some(dunce::simplified(&path).to_path_buf())
        );
        assert_eq!(doc.as_str(), uri.as_str());
    }

    #[test]
    fn a_non_file_uri_has_no_path() {
        let uri = DocUri::try_from("untitled:buffer.py").expect("well-formed uri");

        assert_eq!(uri.to_path(), None);
    }

    #[test]
    fn a_relative_or_malformed_address_is_invalid_params() {
        for value in ["a.py", "/tmp/a.py", "http://[::1"] {
            let Err(error) = DocUri::try_from(value) else {
                panic!("{value} must not parse as a document URI");
            };
            assert!(
                matches!(error, ServerError::InvalidParams(_)),
                "{value} must be rejected as invalid params, got {error:?}"
            );
        }
    }

    #[test]
    fn a_root_must_be_an_existing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = RootDir::try_from(dir.path()).expect("directory root");

        assert!(root.contains(&dir.path().join("a.py")));
        assert!(!root.contains(Path::new("/elsewhere/a.py")));

        let file = dir.path().join("a.py");
        std::fs::write(&file, "x = 1\n").expect("write");
        for rejected in [file.as_path(), &dir.path().join("missing")] {
            let error = RootDir::try_from(rejected).expect_err("must be rejected");
            assert!(
                error.to_string().contains("not an existing directory"),
                "{rejected:?} must be rejected, got {error}"
            );
        }
    }
}
