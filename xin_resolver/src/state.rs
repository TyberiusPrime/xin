//! The three state tables (resolver-architecture.md §1) plus store
//! knowledge. Three facts, three natural keys:
//!
//! - `Mapping` (by `InputHash`): "this recipe resolves to that output" —
//!   naming and building. Byte-identical recipes collapse here (A7).
//! - `Realization` (by `OutputHash`): "these bytes are present locally,
//!   closure and all" — presence and downloading. Runtime-closure expansion
//!   enters here without needing any node identity.
//! - `NodeSlot` (by `NodeId`): demand, blame, human-facing status.
//!
//! Monotonicity rules (§4): facts are inserted, never mutated — a second,
//! differing value is the A8 conflict detector firing, not an overwrite.
//! Demand only grows.

use std::collections::{BTreeMap, BTreeSet};

use crate::events::LeaseId;
use crate::failure::FailureId;
use crate::hashes::{InputHash, OutputHash};
use crate::input::{NodeId, StoreName};

/// Why realization of something is demanded. Monotone; stored reasons drive
/// completion notifications and make the keep-going report explicable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Reason {
    Target(NodeId),
    /// the named node is the designated builder waiting on this input
    BuildInputOf(NodeId),
    /// the named output's runtime closure pulls this in (A7 scope expansion)
    RuntimeOf(OutputHash),
}

#[derive(Clone, Debug, Default)]
pub struct Demand {
    /// naming demand; true for every pruned node (the fully-named DAG is an
    /// end condition), kept explicit per the architecture
    pub name: bool,
    pub realize: BTreeSet<Reason>,
}

/// Per-node bookkeeping: thin by design — the interesting machines are the
/// two hash-keyed ones.
#[derive(Clone, Debug)]
pub struct NodeSlot {
    pub input_hash: Option<InputHash>,
    /// distinct upstream nodes not yet output-named; 0 ⇒ input-hash computable
    pub unnamed_upstreams: u32,
    pub demand: Demand,
    /// direct failures only; upstream blame is derived at report time so it
    /// cannot depend on event arrival order
    pub failure: Option<FailureId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BuildPhase {
    /// realize demands issued on the build inputs; waiting for them
    AwaitingInputs,
    AwaitingLease,
    Running,
    CommittingOutput {
        output: OutputHash,
        runtime_refs: BTreeSet<OutputHash>,
    },
    CommittingMapping {
        output: OutputHash,
        runtime_refs: BTreeSet<OutputHash>,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MappingState {
    /// input-hash known, nothing asked yet
    Unresolved,
    /// waiting for the full answer wave — we only decide on complete waves,
    /// which is what keeps conflict detection order-independent
    Querying {
        pending: BTreeSet<StoreName>,
        answers: Vec<(StoreName, OutputHash)>,
    },
    /// no store knew it, or the bytes exist nowhere and we are the producer.
    /// `known_output` is `Some` on a named-but-absent rebuild; the build
    /// must then reproduce exactly that output (A2/B18 detector).
    Building {
        builder: NodeId,
        known_output: Option<OutputHash>,
        lease: Option<LeaseId>,
        phase: BuildPhase,
    },
    Resolved {
        output: OutputHash,
    },
    Failed(FailureId),
}

#[derive(Clone, Debug)]
pub struct Mapping {
    pub state: MappingState,
    /// nodes sharing this input-hash, ascending — A7's DAG collapse. The
    /// lowest id is the designated builder and takes blame.
    pub nodes: Vec<NodeId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DownloadPhase {
    AwaitingLease,
    Fetching,
    Committing { runtime_refs: BTreeSet<OutputHash> },
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum RealizationState {
    #[default]
    Absent,
    Downloading {
        from: StoreName,
        to: StoreName,
        lease: Option<LeaseId>,
        phase: DownloadPhase,
    },
    Present {
        store: StoreName,
    },
    Failed(FailureId),
}

#[derive(Clone, Debug, Default)]
pub struct Realization {
    pub state: RealizationState,
    pub demand: BTreeSet<Reason>,
    /// presence queries not yet answered; download-vs-build decisions wait
    /// for the whole wave (determinism)
    pub pending_presence: BTreeSet<StoreName>,
    /// facts: which local stores have the bytes / which remotes could serve them
    pub present_in: BTreeSet<StoreName>,
    pub available_in: BTreeSet<StoreName>,
    /// declared runtime refs, once learned (from build, download, or a local
    /// store's presence answer); a fact, inserted once
    pub rt_refs: Option<BTreeSet<OutputHash>>,
    /// runtime deps not yet fully realized; drains monotonically
    pub rt_missing: BTreeSet<OutputHash>,
    /// mapping that produces this output, when one is known — the build
    /// fallback for demanded-but-unavailable bytes
    pub producer: Option<InputHash>,
    /// presence queries have been issued (at most once per output)
    pub queried: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MappingSource {
    Built(NodeId),
    Substituted,
}

/// Positive + negative knowledge about stores (§5). Consulted *before* an
/// effect is emitted, so "don't ask twice" is a core invariant, not an
/// executor courtesy. TTL expiry will arrive as a timer event (later
/// milestone); B5 says this eventually gets a file format.
#[derive(Clone, Debug, Default)]
pub struct StoreKnowledge {
    /// the learned input→output facts; insert-only
    pub facts: BTreeMap<InputHash, (OutputHash, MappingSource)>,
    pub asked_mappings: BTreeSet<(StoreName, InputHash)>,
    pub asked_presence: BTreeSet<(StoreName, OutputHash)>,
}
