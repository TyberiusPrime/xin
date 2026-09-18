//! The TOML intermediary — the serialized form of the resolver's ingest
//! boundary. Nickel evaluation produces it; plain hand-written TOML is
//! equally valid input. Parsing turns it into a `RawInput` (which
//! `ingest()` then validates: names, cycles, store references) plus the
//! store *locations*, which the resolver does not care about but the
//! driver/CLI does.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use xin_resolver::input::{
    BuilderType, Cores, HumanName, InputName, RawInput, RawNode, StoreDef, StoreName,
    ValidRemoteStores,
};

use crate::DagError;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TomlDag {
    pub primary: Option<String>,
    /// usually empty — stores live in xin.config.toml and get merged in via
    /// `merge_config`; inline definitions keep single-file DAGs possible
    #[serde(default)]
    pub stores: BTreeMap<String, TomlStore>,
    pub nodes: BTreeMap<String, TomlNode>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TomlStore {
    Local {
        path: String,
        #[serde(default = "default_true")]
        writeable: bool,
    },
    Remote {
        url: String,
    },
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TomlNode {
    #[serde(default = "default_builder")]
    pub builder: String,
    pub recipe: String,
    #[serde(default)]
    pub target: bool,
    /// local alias -> upstream node name
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    #[serde(default = "default_cores")]
    pub cores: String,
    #[serde(default = "default_remotes")]
    pub remotes: TomlRemotes,
    pub target_store: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum TomlRemotes {
    Word(String),
    Allow { allow: Vec<String> },
    Deny { deny: Vec<String> },
}

fn default_true() -> bool {
    true
}
fn default_builder() -> String {
    "process".into()
}
fn default_cores() -> String {
    "one".into()
}
fn default_remotes() -> TomlRemotes {
    TomlRemotes::Word("all".into())
}

/// Where a store's bytes actually live — driver/CLI material, opaque to the
/// resolver core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreLocation {
    Local { path: PathBuf, writeable: bool },
    Remote { url: String },
}

#[derive(Debug)]
pub struct DagBundle {
    /// unvalidated ingest input; call `.ingest()` for the pruned DAG
    pub raw: RawInput,
    pub locations: BTreeMap<StoreName, StoreLocation>,
}

/// Rebase relative local-store paths onto the directory their defining file
/// lives in, so cwd never influences where a store ends up.
pub fn absolutize_stores(stores: &mut BTreeMap<String, TomlStore>, base: &std::path::Path) {
    for store in stores.values_mut() {
        if let TomlStore::Local { path, .. } = store {
            let p = std::path::Path::new(path.as_str());
            if p.is_relative() {
                // component-wise join normalizes away "./" without touching
                // the filesystem (the store may not exist yet)
                let mut joined = base.to_path_buf();
                for c in p.components() {
                    match c {
                        std::path::Component::CurDir => {}
                        c => joined.push(c),
                    }
                }
                *path = joined.to_string_lossy().into_owned();
            }
        }
    }
}

impl TomlDag {
    /// Fold the config file's stores (and primary, if the DAG names none)
    /// into this DAG. A store defined in both places is an error, not a
    /// shadowing rule.
    pub fn merge_config(&mut self, cfg: &crate::config::XinConfig) -> Result<(), DagError> {
        for (name, store) in &cfg.stores {
            if self.stores.contains_key(name) {
                return Err(DagError::Value(format!(
                    "store {name} is defined both in the DAG file and in {}",
                    cfg.path.display()
                )));
            }
            self.stores.insert(name.clone(), store.clone());
        }
        if self.primary.is_none() {
            self.primary = cfg.primary.clone();
        }
        Ok(())
    }

    pub fn into_bundle(self) -> Result<DagBundle, DagError> {
        let store_name = |s: &str| StoreName::new(s).map_err(|e| DagError::Value(format!("{e:?}")));

        // pick the primary: explicit, or the unique writeable local store
        let writeable_locals: Vec<&String> = self
            .stores
            .iter()
            .filter(|(_, d)| {
                matches!(
                    d,
                    TomlStore::Local {
                        writeable: true,
                        ..
                    }
                )
            })
            .map(|(n, _)| n)
            .collect();
        let primary = match &self.primary {
            Some(p) => match self.stores.get(p) {
                Some(TomlStore::Local {
                    writeable: true, ..
                }) => p.clone(),
                Some(_) => {
                    return Err(DagError::Value(format!(
                        "primary store {p} must be a writeable local store"
                    )));
                }
                None => return Err(DagError::Value(format!("primary store {p} is not defined"))),
            },
            None => match writeable_locals.as_slice() {
                [only] => (*only).clone(),
                [] => {
                    return Err(DagError::Value(
                        "no writeable local store defined; the resolver needs a primary".into(),
                    ));
                }
                _ => {
                    return Err(DagError::Value(format!(
                        "multiple writeable local stores ({}); set `primary` explicitly",
                        writeable_locals
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            },
        };

        // store order = resolver ranking: primary first, then name order
        let mut store_defs: Vec<(StoreName, StoreDef)> = Vec::new();
        let mut locations = BTreeMap::new();
        let mut ordered: Vec<&String> = self.stores.keys().collect();
        ordered.sort_by_key(|n| (**n != primary, (*n).clone()));
        for name in ordered {
            let sname = store_name(name)?;
            let (def, loc) = match &self.stores[name] {
                TomlStore::Local { path, writeable } => (
                    StoreDef::Local {
                        writeable: *writeable,
                    },
                    StoreLocation::Local {
                        path: PathBuf::from(path),
                        writeable: *writeable,
                    },
                ),
                TomlStore::Remote { url } => {
                    (StoreDef::Remote, StoreLocation::Remote { url: url.clone() })
                }
            };
            store_defs.push((sname.clone(), def));
            locations.insert(sname, loc);
        }

        let mut nodes: Vec<(HumanName, RawNode)> = Vec::new();
        for (name, tn) in &self.nodes {
            let hname = HumanName::new(name).map_err(|e| DagError::Value(format!("{e:?}")))?;
            let builder = match tn.builder.as_str() {
                "process" => BuilderType::Process,
                "fetchurl" => BuilderType::FetchUrl,
                other => {
                    return Err(DagError::Value(format!(
                        "node {name}: unknown builder {other:?} (expected \"process\" or \"fetchurl\")"
                    )));
                }
            };
            let cores = match tn.cores.as_str() {
                "one" => Cores::One,
                "all" => Cores::All,
                other => {
                    return Err(DagError::Value(format!(
                        "node {name}: unknown cores {other:?} (expected \"one\" or \"all\")"
                    )));
                }
            };
            let remotes = match &tn.remotes {
                TomlRemotes::Word(w) if w == "all" => ValidRemoteStores::All,
                TomlRemotes::Word(w) if w == "none" => ValidRemoteStores::None,
                TomlRemotes::Word(other) => {
                    return Err(DagError::Value(format!(
                        "node {name}: remotes must be \"all\", \"none\", {{ allow = [..] }} or {{ deny = [..] }}, found {other:?}"
                    )));
                }
                TomlRemotes::Allow { allow } => ValidRemoteStores::Allow(
                    allow
                        .iter()
                        .map(|s| store_name(s))
                        .collect::<Result<_, _>>()?,
                ),
                TomlRemotes::Deny { deny } => ValidRemoteStores::Deny(
                    deny.iter()
                        .map(|s| store_name(s))
                        .collect::<Result<_, _>>()?,
                ),
            };
            let mut upstreams = Vec::new();
            for (alias, upstream) in &tn.inputs {
                upstreams.push((
                    InputName::new(alias).map_err(|e| DagError::Value(format!("{e:?}")))?,
                    HumanName::new(upstream).map_err(|e| DagError::Value(format!("{e:?}")))?,
                ));
            }
            nodes.push((
                hname,
                RawNode {
                    builder,
                    recipe: tn.recipe.clone().into_bytes(),
                    is_target: tn.target,
                    target_store: tn.target_store.as_deref().map(store_name).transpose()?,
                    remotes,
                    upstreams,
                    cores,
                },
            ));
        }

        Ok(DagBundle {
            raw: RawInput::from_parts(nodes, store_defs),
            locations,
        })
    }
}
