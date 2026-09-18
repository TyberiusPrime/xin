//! `xin.config.toml` — machine/project configuration, deliberately *not*
//! part of the DAG definition: which stores exist and where, which one is
//! primary, and policy decisions (failure policy, results folder). DAG
//! files describe *what* to build; the config describes *where and how*
//! this machine builds it, so the same DAG file works across machines.
//!
//! Discovery walks upward from the working directory (like git), so
//! invoking `xin` anywhere inside a project finds the project config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::DagError;
use crate::schema::{TomlStore, absolutize_stores};

pub const CONFIG_FILE: &str = "xin.config.toml";

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    primary: Option<String>,
    /// where target symlinks land; relative to the config file's directory
    results: Option<String>,
    on_failure: Option<String>,
    bootstrap: Option<String>,
    #[serde(default)]
    stores: BTreeMap<String, TomlStore>,
}

/// A7: keep-going is the default; fail-fast is a host scheduling policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnFailure {
    KeepGoing,
    FailFast,
}

#[derive(Debug)]
pub struct XinConfig {
    /// the config file itself (for error messages)
    pub path: PathBuf,
    /// its directory — the base for every relative path in the file
    pub dir: PathBuf,
    pub primary: Option<String>,
    /// absolute after load; defaults to `<dir>/results` (A7)
    pub results: PathBuf,
    pub on_failure: OnFailure,
    /// path to the static bootstrap busybox for the (mandatory) build
    /// sandbox; absolute after load. None = $XIN_BOOTSTRAP / PATH discovery
    pub bootstrap: Option<PathBuf>,
    /// local paths are absolute after load
    pub stores: BTreeMap<String, TomlStore>,
}

impl XinConfig {
    /// Walk upward from `start` looking for `xin.config.toml`.
    pub fn find(start: &Path) -> Option<PathBuf> {
        let mut dir = start;
        loop {
            let candidate = dir.join(CONFIG_FILE);
            if candidate.is_file() {
                return Some(candidate);
            }
            dir = dir.parent()?;
        }
    }

    pub fn load(path: &Path) -> Result<XinConfig, DagError> {
        let text = std::fs::read_to_string(path).map_err(DagError::Io)?;
        let raw: TomlConfig = toml::from_str(&text).map_err(|e| DagError::Toml(Box::new(e)))?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = dir.canonicalize().map_err(DagError::Io)?;
        let on_failure = match raw.on_failure.as_deref() {
            None | Some("keep-going") => OnFailure::KeepGoing,
            Some("fail-fast") => OnFailure::FailFast,
            Some(other) => {
                return Err(DagError::Value(format!(
                    "{}: on_failure must be \"keep-going\" or \"fail-fast\", found {other:?}",
                    path.display()
                )));
            }
        };
        let bootstrap = raw.bootstrap.map(|b| {
            let b = PathBuf::from(b);
            if b.is_relative() { dir.join(b) } else { b }
        });
        let results = {
            let r = PathBuf::from(raw.results.as_deref().unwrap_or("results"));
            if r.is_relative() { dir.join(r) } else { r }
        };
        let mut stores = raw.stores;
        absolutize_stores(&mut stores, &dir);
        Ok(XinConfig {
            path: path.to_path_buf(),
            dir,
            primary: raw.primary,
            results,
            on_failure,
            bootstrap,
            stores,
        })
    }
}
