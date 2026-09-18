use std::path::PathBuf;

use indexmap::IndexMap;
use petgraph::graph::DiGraph;

pub(crate) struct HumanName(String);
pub(crate) struct StoreName(String);

pub(crate) enum BuilderType {
    Process,
    FetchUrl,
}

// dag nodes
pub(crate) struct Node {
    // human_name: String, encoded in the Input hashmap
    builder: BuilderType,
    recipe: Vec<u8>,
    is_target: bool,
    target_store: StoreName,
    remotes: ValidRemoteStores,
    upstream_nodes: Vec<HumanName>,
}

pub(crate) enum TargetStore {
    Primary,
    Other(StoreName),
}
pub(crate) enum ValidRemoteStores {
    None,
    All,
    Allow(Vec<StoreName>), // accept only these
    Deny(Vec<StoreName>),  // accept all but these.
}

pub(crate) enum Store {
    LocalStore,
    RemoteStore,
}

// stores
pub(crate) struct LocalStore {
    path: PathBuf,
    writeable: bool,
}
pub(crate) struct RemoteStore {
    url: String,
}

//todo: interned (human) Node Ids

pub type NodeId = u32;
pub(crate) struct Input {
    nodes: DiGraph<NodeId, Node>,
    stores: IndexMap<StoreName, Store>,
}

impl Input {
    pub fn prune_to_targets(&self) -> Self {
        todo!();
    }
}
