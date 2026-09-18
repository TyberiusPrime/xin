//! The common front half of every command: find the config, find the DAG
//! file, evaluate/parse it, merge, bundle, and stand up a driver.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use xin_dag::config::{CONFIG_FILE, XinConfig};
use xin_dag::schema::absolutize_stores;
use xin_dag::{DagBundle, DagError, StoreLocation};
use xin_driver::{Backend, Driver, LocalStore};
use xin_resolver::Resolver;
use xin_resolver::input::Dag;

pub const DEFAULT_EXT: &str = ".xin.ncl";

/// Explicit `--config` must exist; otherwise search upward from the cwd,
/// and running without any config is fine as long as the DAG file defines
/// its stores inline.
pub fn load_config(explicit: Option<&Path>) -> Result<Option<XinConfig>, String> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            XinConfig::find(&cwd)
        }
    };
    path.map(|p| XinConfig::load(&p).map_err(|e| e.to_string()))
        .transpose()
}

/// The config for commands that cannot work without one (gc, store).
pub fn require_config(explicit: Option<&Path>) -> Result<XinConfig, String> {
    load_config(explicit)?.ok_or_else(|| {
        format!("no {CONFIG_FILE} found (searched upward from the current directory); stores are configured there")
    })
}

/// `foo` means `foo{DEFAULT_EXT}`; explicit .ncl/.toml paths are taken as
/// given; no argument means the unique `*.xin.ncl` in the cwd.
pub fn resolve_dag_path(arg: Option<&str>) -> Result<PathBuf, String> {
    match arg {
        Some(a) => {
            let p = PathBuf::from(a);
            if matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ncl") | Some("toml")
            ) {
                return if p.is_file() {
                    Ok(p)
                } else {
                    Err(format!("{a}: no such file"))
                };
            }
            let with = PathBuf::from(format!("{a}{DEFAULT_EXT}"));
            if with.is_file() {
                Ok(with)
            } else {
                Err(format!("no DAG named {a}: tried {}", with.display()))
            }
        }
        None => {
            let mut found: Vec<PathBuf> = fs::read_dir(".")
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(DEFAULT_EXT))
                })
                .collect();
            found.sort();
            match found.as_slice() {
                [one] => Ok(one.clone()),
                [] => Err(format!(
                    "no *{DEFAULT_EXT} file in the current directory; pass a DAG file"
                )),
                many => Err(format!(
                    "several DAG files here ({}); pick one",
                    many.iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
    }
}

pub struct LoadedDag {
    pub path: PathBuf,
    pub bundle: DagBundle,
}

/// DAG file → merged, bundled definition. Inline store paths resolve
/// relative to the DAG file, config store paths already resolved relative
/// to the config — the cwd never decides where a store lands.
pub fn load_bundle(
    file_arg: Option<&str>,
    config: Option<&XinConfig>,
) -> Result<LoadedDag, String> {
    let path = resolve_dag_path(file_arg)?;
    let (mut dag, _toml_text) = xin_dag::load_dag(&path).map_err(|e| e.to_string())?;
    let dag_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let dag_dir = dag_dir.canonicalize().map_err(|e| e.to_string())?;
    absolutize_stores(&mut dag.stores, &dag_dir);
    if let Some(cfg) = config {
        dag.merge_config(cfg).map_err(|e| e.to_string())?;
    }
    let bundle = dag
        .into_bundle()
        .map_err(|e| match e {
            DagError::Value(msg) if msg.contains("no writeable local store") => DagError::Value(
                format!("{msg} — define stores in {CONFIG_FILE} or inline in the DAG file"),
            ),
            e => e,
        })
        .map_err(|e| e.to_string())?;
    Ok(LoadedDag { path, bundle })
}

/// Backends in bundle-store order — which is the resolver's ranking order.
pub fn make_driver(bundle: &DagBundle) -> io::Result<Driver> {
    let mut backends = Vec::new();
    for (name, _) in &bundle.raw.stores {
        let backend = match bundle.locations.get(name) {
            Some(StoreLocation::Local { path, .. }) => Backend::Local(LocalStore::open(path)?),
            Some(StoreLocation::Remote { .. }) | None => Backend::DummyRemote,
        };
        backends.push((name.clone(), backend));
    }
    Ok(Driver::new(backends))
}

/// Ingest and wrap; ingest errors are user errors (bad DAG), not bugs.
pub fn make_resolver(bundle: &LoadedDag) -> Result<Resolver, String> {
    let dag: Dag = bundle
        .bundle
        .raw
        .clone()
        .ingest()
        .map_err(|e| format!("invalid DAG in {}: {e:?}", bundle.path.display()))?;
    Ok(Resolver::new(dag))
}

/// Open every writeable local store from a config (the gc set).
pub fn open_config_stores(cfg: &XinConfig) -> Result<Vec<(String, LocalStore)>, String> {
    use xin_dag::schema::TomlStore;
    let mut out = Vec::new();
    for (name, def) in &cfg.stores {
        if let TomlStore::Local {
            path,
            writeable: true,
        } = def
        {
            out.push((
                name.clone(),
                LocalStore::open(path).map_err(|e| format!("store {name} at {path}: {e}"))?,
            ));
        }
    }
    if out.is_empty() {
        return Err(format!(
            "{}: no writeable local stores configured",
            cfg.path.display()
        ));
    }
    Ok(out)
}
