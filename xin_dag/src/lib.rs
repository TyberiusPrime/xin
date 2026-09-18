//! xin_dag — the DAG definition layer: Nickel in, TOML intermediary out,
//! `RawInput` + store locations into the resolver/driver.
//!
//! The pipeline is `user.ncl` --(embedded nickel eval, `xin.ncl` prelude
//! contract)--> TOML --(serde `schema`)--> `DagBundle { RawInput,
//! locations }` --(`RawInput::ingest`)--> pruned, validated `Dag`. The TOML
//! step is a real interchange format, not just an implementation detail:
//! hand-written TOML is accepted directly, and the CLI can emit the
//! intermediary for inspection or caching.
//!
//! Still out of scope here (tracked in design.md): TOFU hash write-back
//! into the definitions (A2) and the special-input canonicalization beyond
//! "the recipe string is the opaque C1 payload".

use std::io;
use std::path::Path;

pub mod config;
pub mod nickel;
pub mod schema;

pub use config::{OnFailure, XinConfig};
pub use schema::{DagBundle, StoreLocation, TomlDag};

#[derive(Debug)]
pub enum DagError {
    Io(io::Error),
    /// rendered Nickel diagnostics (contract failures, parse errors, ...)
    Nickel(String),
    Toml(Box<toml::de::Error>),
    /// semantic values the schema rejects (unknown builder, bad primary, ...)
    Value(String),
    UnknownExtension(String),
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::Io(e) => write!(f, "io error: {e}"),
            DagError::Nickel(msg) => write!(f, "nickel evaluation failed:\n{msg}"),
            DagError::Toml(e) => write!(f, "invalid DAG toml: {e}"),
            DagError::Value(msg) => write!(f, "invalid DAG definition: {msg}"),
            DagError::UnknownExtension(p) => {
                write!(f, "cannot load {p}: expected a .ncl or .toml file")
            }
        }
    }
}

impl std::error::Error for DagError {}

/// Parse the TOML intermediary into a bundle.
pub fn parse_toml(text: &str) -> Result<DagBundle, DagError> {
    let dag: TomlDag = toml::from_str(text).map_err(|e| DagError::Toml(Box::new(e)))?;
    dag.into_bundle()
}

/// Load a DAG definition to the TOML-intermediary stage by extension:
/// `.ncl` is evaluated through Nickel, `.toml` is parsed directly. The
/// parsed `TomlDag` still has config merging (`merge_config`) and bundling
/// (`into_bundle`) ahead of it; the returned text is the intermediary for
/// `xin eval`-style inspection.
pub fn load_dag(path: &Path) -> Result<(TomlDag, String), DagError> {
    let text = match path.extension().and_then(|e| e.to_str()) {
        Some("ncl") => nickel::eval_to_toml(path, &[])?,
        Some("toml") => std::fs::read_to_string(path).map_err(DagError::Io)?,
        _ => return Err(DagError::UnknownExtension(path.display().to_string())),
    };
    let dag: TomlDag = toml::from_str(&text).map_err(|e| DagError::Toml(Box::new(e)))?;
    Ok((dag, text))
}

/// Load a self-contained DAG definition (stores defined inline) into a
/// bundle. The CLI path goes through `load_dag` + `merge_config` instead.
pub fn load(path: &Path) -> Result<(DagBundle, String), DagError> {
    let (dag, text) = load_dag(path)?;
    Ok((dag.into_bundle()?, text))
}
