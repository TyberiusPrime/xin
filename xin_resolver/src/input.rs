//! The resolver's ingest boundary (feedback blocker 4): the types the DAG
//! frontend hands over, plus validation / pruning / interning into the
//! immutable `Dag` the core runs on.
//!
//! Interning is deterministic and *topological*: `NodeId`s are assigned in a
//! topo order (ties broken by definition order), so an upstream's id is
//! always smaller than its downstream's. That makes "pick the lowest NodeId"
//! tie-breaks stable and lets report generation walk ids in ascending order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use indexmap::IndexMap;

/// Node names from the DAG frontend. C0 restricts the charset; the
/// resolver-plan glossary additionally allows '_', which we follow.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct HumanName(String);

/// The *local alias* a recipe uses for one of its inputs (C1): part of the
/// input-hash preimage, and not necessarily equal to the upstream's name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct InputName(String);

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StoreName(String);

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl HumanName {
    pub fn new(s: &str) -> Result<Self, IngestError> {
        if valid_name(s) {
            Ok(HumanName(s.to_owned()))
        } else {
            Err(IngestError::BadName(s.to_owned()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl InputName {
    pub fn new(s: &str) -> Result<Self, IngestError> {
        if valid_name(s) {
            Ok(InputName(s.to_owned()))
        } else {
            Err(IngestError::BadName(s.to_owned()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl StoreName {
    pub fn new(s: &str) -> Result<Self, IngestError> {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_graphic()) {
            Ok(StoreName(s.to_owned()))
        } else {
            Err(IngestError::BadName(s.to_owned()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HumanName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for InputName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for StoreName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuilderType {
    Process,
    /// fetchers are builders too (A2); which one is data, not a trait method
    FetchUrl,
}

/// B11: a build needs one core or all of them — defined, not discovered.
/// Host-scheduler concern; opaque to the core.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cores {
    One,
    All,
}

/// A10: per-node hint which remote stores are worth asking / trusting.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValidRemoteStores {
    None,
    All,
    Allow(Vec<StoreName>),
    Deny(Vec<StoreName>),
}

impl ValidRemoteStores {
    pub fn allows(&self, s: &StoreName) -> bool {
        match self {
            ValidRemoteStores::None => false,
            ValidRemoteStores::All => true,
            ValidRemoteStores::Allow(v) => v.contains(s),
            ValidRemoteStores::Deny(v) => !v.contains(s),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StoreDef {
    Local { writeable: bool },
    Remote,
}

/// One node as the DAG frontend hands it over.
#[derive(Clone, Debug)]
pub struct RawNode {
    pub builder: BuilderType,
    /// opaque special-input bytes (C1): script, env, fetcher+url, ...
    pub recipe: Vec<u8>,
    pub is_target: bool,
    /// None = the primary store. A4: must be a machine-local store —
    /// checked at ingest time, not at build time.
    pub target_store: Option<StoreName>,
    pub remotes: ValidRemoteStores,
    pub upstreams: Vec<(InputName, HumanName)>,
    pub cores: Cores,
}

#[derive(Clone, Debug)]
pub struct RawInput {
    pub nodes: IndexMap<HumanName, RawNode>,
    pub stores: IndexMap<StoreName, StoreDef>,
}

impl RawInput {
    pub fn from_parts(
        nodes: Vec<(HumanName, RawNode)>,
        stores: Vec<(StoreName, StoreDef)>,
    ) -> RawInput {
        RawInput {
            nodes: nodes.into_iter().collect(),
            stores: stores.into_iter().collect(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(pub u32);

impl NodeId {
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug)]
pub struct DagNode {
    pub name: HumanName,
    pub builder: BuilderType,
    pub recipe: Vec<u8>,
    pub is_target: bool,
    pub target_store: StoreName,
    pub remotes: ValidRemoteStores,
    /// sorted by alias, byte-wise — the C1 preimage order
    pub upstreams: Vec<(InputName, NodeId)>,
    /// distinct downstream nodes, ascending
    pub downstreams: Vec<NodeId>,
    pub cores: Cores,
}

impl DagNode {
    /// distinct upstream *nodes* (two aliases may point at the same node)
    pub fn distinct_upstreams(&self) -> BTreeSet<NodeId> {
        self.upstreams.iter().map(|(_, u)| *u).collect()
    }
}

/// The pruned, validated, interned DAG. Immutable once built.
#[derive(Clone, Debug)]
pub struct Dag {
    /// index = NodeId; topologically ordered (upstream id < downstream id)
    pub nodes: Vec<DagNode>,
    pub stores: IndexMap<StoreName, StoreDef>,
    pub primary: StoreName,
    pub targets: Vec<NodeId>,
}

impl Dag {
    pub fn id_of(&self, name: &str) -> Option<NodeId> {
        self.nodes
            .iter()
            .position(|n| n.name.as_str() == name)
            .map(|i| NodeId(i as u32))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IngestError {
    BadName(String),
    UnknownUpstream { node: String, upstream: String },
    DuplicateInputName { node: String, input: String },
    Cycle(Vec<String>),
    NoTargets,
    NoPrimaryStore,
    UnknownStore { node: String, store: String },
    TargetStoreNotLocal { node: String, store: String },
}

impl RawInput {
    /// Validate, prune to the targets' upstream closure, intern names to
    /// dense topo-ordered `NodeId`s.
    pub fn ingest(self) -> Result<Dag, IngestError> {
        let RawInput { nodes, stores } = self;

        let primary = stores
            .iter()
            .find(|(_, d)| matches!(d, StoreDef::Local { writeable: true }))
            .map(|(n, _)| n.clone())
            .ok_or(IngestError::NoPrimaryStore)?;

        let raw: Vec<(&HumanName, &RawNode)> = nodes.iter().collect();
        let index: BTreeMap<&HumanName, usize> =
            raw.iter().enumerate().map(|(i, (n, _))| (*n, i)).collect();

        for (name, rn) in &raw {
            let mut seen = BTreeSet::new();
            for (iname, up) in &rn.upstreams {
                if !index.contains_key(up) {
                    return Err(IngestError::UnknownUpstream {
                        node: name.to_string(),
                        upstream: up.to_string(),
                    });
                }
                if !seen.insert(iname) {
                    return Err(IngestError::DuplicateInputName {
                        node: name.to_string(),
                        input: iname.to_string(),
                    });
                }
            }
            if let Some(ts) = &rn.target_store {
                match stores.get(ts) {
                    Option::None => {
                        return Err(IngestError::UnknownStore {
                            node: name.to_string(),
                            store: ts.to_string(),
                        });
                    }
                    Some(StoreDef::Remote) => {
                        return Err(IngestError::TargetStoreNotLocal {
                            node: name.to_string(),
                            store: ts.to_string(),
                        });
                    }
                    Some(StoreDef::Local { .. }) => {}
                }
            }
            if let ValidRemoteStores::Allow(v) | ValidRemoteStores::Deny(v) = &rn.remotes {
                for s in v {
                    if !stores.contains_key(s) {
                        return Err(IngestError::UnknownStore {
                            node: name.to_string(),
                            store: s.to_string(),
                        });
                    }
                }
            }
        }

        let target_idx: Vec<usize> = raw
            .iter()
            .enumerate()
            .filter(|(_, (_, rn))| rn.is_target)
            .map(|(i, _)| i)
            .collect();
        if target_idx.is_empty() {
            return Err(IngestError::NoTargets);
        }

        // prune: upstream closure of the targets
        let mut keep = vec![false; raw.len()];
        let mut stack = target_idx.clone();
        while let Some(i) = stack.pop() {
            if !keep[i] {
                keep[i] = true;
                for (_, up) in &raw[i].1.upstreams {
                    stack.push(index[up]);
                }
            }
        }
        let kept_count = keep.iter().filter(|k| **k).count();

        // distinct-upstream adjacency over the kept subgraph
        let distinct_ups: Vec<BTreeSet<usize>> = raw
            .iter()
            .map(|(_, rn)| rn.upstreams.iter().map(|(_, u)| index[u]).collect())
            .collect();
        let mut downs: Vec<Vec<usize>> = vec![Vec::new(); raw.len()];
        for i in 0..raw.len() {
            if keep[i] {
                for &u in &distinct_ups[i] {
                    downs[u].push(i);
                }
            }
        }

        // Kahn with a BTreeSet ready-queue: topo order, deterministic ties
        let mut indeg: Vec<usize> = distinct_ups.iter().map(|u| u.len()).collect();
        let mut ready: BTreeSet<usize> = (0..raw.len())
            .filter(|&i| keep[i] && indeg[i] == 0)
            .collect();
        let mut order: Vec<usize> = Vec::with_capacity(kept_count);
        let mut newid = vec![usize::MAX; raw.len()];
        while let Some(&i) = ready.iter().next() {
            ready.remove(&i);
            newid[i] = order.len();
            order.push(i);
            for &d in &downs[i] {
                indeg[d] -= 1;
                if indeg[d] == 0 {
                    ready.insert(d);
                }
            }
        }
        if order.len() != kept_count {
            let mut cyc: Vec<String> = (0..raw.len())
                .filter(|&i| keep[i] && newid[i] == usize::MAX)
                .map(|i| raw[i].0.to_string())
                .collect();
            cyc.sort();
            return Err(IngestError::Cycle(cyc));
        }

        let mut dag_nodes: Vec<DagNode> = Vec::with_capacity(order.len());
        for &i in &order {
            let (name, rn) = raw[i];
            let mut ups: Vec<(InputName, NodeId)> = rn
                .upstreams
                .iter()
                .map(|(inm, up)| (inm.clone(), NodeId(newid[index[up]] as u32)))
                .collect();
            ups.sort_by(|a, b| a.0.cmp(&b.0));
            dag_nodes.push(DagNode {
                name: name.clone(),
                builder: rn.builder,
                recipe: rn.recipe.clone(),
                is_target: rn.is_target,
                target_store: rn.target_store.clone().unwrap_or_else(|| primary.clone()),
                remotes: rn.remotes.clone(),
                upstreams: ups,
                downstreams: Vec::new(),
                cores: rn.cores,
            });
        }
        for id in 0..dag_nodes.len() {
            for up in dag_nodes[id].distinct_upstreams() {
                dag_nodes[up.idx()].downstreams.push(NodeId(id as u32));
            }
        }

        let mut targets: Vec<NodeId> = target_idx
            .iter()
            .map(|&i| NodeId(newid[i] as u32))
            .collect();
        targets.sort();

        Ok(Dag {
            nodes: dag_nodes,
            stores,
            primary,
            targets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hn(s: &str) -> HumanName {
        HumanName::new(s).unwrap()
    }

    fn node(ups: &[(&str, &str)], target: bool) -> RawNode {
        RawNode {
            builder: BuilderType::Process,
            recipe: b"r".to_vec(),
            is_target: target,
            target_store: Option::None,
            remotes: ValidRemoteStores::All,
            upstreams: ups
                .iter()
                .map(|(a, u)| (InputName::new(a).unwrap(), hn(u)))
                .collect(),
            cores: Cores::One,
        }
    }

    fn one_store() -> Vec<(StoreName, StoreDef)> {
        vec![(
            StoreName::new("primary").unwrap(),
            StoreDef::Local { writeable: true },
        )]
    }

    #[test]
    fn names_validated() {
        assert!(HumanName::new("ok-name_2").is_ok());
        assert!(HumanName::new("bad name").is_err());
        assert!(HumanName::new("").is_err());
        assert!(HumanName::new("new\nline").is_err());
    }

    #[test]
    fn prune_and_topo_intern() {
        let raw = RawInput::from_parts(
            vec![
                (hn("top"), node(&[("l", "left"), ("r", "right")], true)),
                (hn("left"), node(&[("b", "base")], false)),
                (hn("right"), node(&[("b", "base")], false)),
                (hn("base"), node(&[], false)),
                (hn("unrelated"), node(&[], false)),
            ],
            one_store(),
        );
        let dag = raw.ingest().unwrap();
        assert_eq!(dag.nodes.len(), 4);
        assert!(dag.id_of("unrelated").is_none());
        // topo: every upstream id < node id
        for (i, n) in dag.nodes.iter().enumerate() {
            for (_, up) in &n.upstreams {
                assert!(up.idx() < i);
            }
        }
        assert_eq!(dag.targets, vec![dag.id_of("top").unwrap()]);
    }

    #[test]
    fn cycle_detected() {
        let raw = RawInput::from_parts(
            vec![
                (hn("a"), node(&[("x", "b")], true)),
                (hn("b"), node(&[("x", "a")], false)),
            ],
            one_store(),
        );
        assert_eq!(
            raw.ingest().unwrap_err(),
            IngestError::Cycle(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn unknown_upstream_detected() {
        let raw = RawInput::from_parts(vec![(hn("a"), node(&[("x", "ghost")], true))], one_store());
        assert!(matches!(
            raw.ingest().unwrap_err(),
            IngestError::UnknownUpstream { .. }
        ));
    }
}
