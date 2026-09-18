//! The driver/executor boundary (A13, resolver-architecture.md §2): every
//! `Effect` is fire-and-forget with a correlation token; every executor
//! reply is an `Event`. The core emits one intent per hash — batching and
//! per-store coalescing are host-scheduler concerns (A10, §5).

use std::collections::BTreeSet;

use crate::failure::BuildLog;
use crate::hashes::{InputHash, OutputHash};
use crate::input::{BuilderType, InputName, NodeId, StoreName};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct LeaseId(pub u64);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// A5: mappings are queried by input-hash
    QueryMapping {
        tok: RequestId,
        store: StoreName,
        input: InputHash,
    },
    /// A5: availability/presence is queried by output-hash
    QueryPresence {
        tok: RequestId,
        store: StoreName,
        output: OutputHash,
    },
    /// A6: GC protection, acquired *before* the build/download effect and
    /// held until the terminal state; an explicit state, not an RAII guard,
    /// so the simulator can run GC mid-build
    AcquireLease {
        tok: RequestId,
        store: StoreName,
        protect: BTreeSet<OutputHash>,
    },
    ReleaseLease {
        store: StoreName,
        lease: LeaseId,
    },
    StartBuild {
        tok: RequestId,
        node: NodeId,
        input: InputHash,
        builder: BuilderType,
        recipe: Vec<u8>,
        store: StoreName,
        /// alias → output-hash, in C1 (alias-sorted) order
        inputs: Vec<(InputName, OutputHash)>,
    },
    StartDownload {
        tok: RequestId,
        output: OutputHash,
        from: StoreName,
        to: StoreName,
    },
    /// commit point 1 (ack-gated): rename(temp → outputs/<oh>);
    /// ENOTEMPTY means a concurrent build won — same bytes, still success
    CommitOutput {
        tok: RequestId,
        store: StoreName,
        output: OutputHash,
    },
    /// commit point 2 (ack-gated): symlink(input/<ih> → outputs/<oh>);
    /// EEXIST with a different target is the nondeterminism detector and
    /// must come back as `CommitResult::Conflict`, not an IO-error string
    CommitMapping {
        tok: RequestId,
        store: StoreName,
        input: InputHash,
        output: OutputHash,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    MappingAnswered {
        tok: RequestId,
        output: Option<OutputHash>,
    },
    PresenceAnswered {
        tok: RequestId,
        answer: PresenceAnswer,
    },
    LeaseGranted {
        tok: RequestId,
        lease: LeaseId,
    },
    BuildFinished {
        tok: RequestId,
        outcome: BuildOutcome,
    },
    DownloadFinished {
        tok: RequestId,
        outcome: DownloadOutcome,
    },
    OutputCommitted {
        tok: RequestId,
        already_existed: bool,
    },
    MappingCommitted {
        tok: RequestId,
        result: CommitResult,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PresenceAnswer {
    pub present: bool,
    /// local stores also report the stored runtime refs (the runtime-inputs/
    /// symlink dir is right there); remotes report bare availability
    pub runtime_refs: Option<BTreeSet<OutputHash>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BuildOutcome {
    /// §10: the outcome carries the output *hash* plus the declared runtime
    /// refs — the second is what feeds the closure validation
    Success {
        output: OutputHash,
        runtime_refs: BTreeSet<OutputHash>,
        log: BuildLog,
    },
    Failure {
        log: BuildLog,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DownloadOutcome {
    /// CAS-verified by the executor; runtime refs read from the fetched tree
    Success {
        runtime_refs: BTreeSet<OutputHash>,
    },
    Failure {
        error: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitResult {
    Committed,
    Conflict { existing: OutputHash },
}

/// What an outstanding `RequestId` was for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Inflight {
    MappingQuery {
        store: StoreName,
        input: InputHash,
    },
    PresenceQuery {
        store: StoreName,
        output: OutputHash,
    },
    BuildLease {
        input: InputHash,
    },
    Build {
        input: InputHash,
    },
    BuildOutputCommit {
        input: InputHash,
    },
    BuildMappingCommit {
        input: InputHash,
    },
    DownloadLease {
        output: OutputHash,
    },
    Download {
        output: OutputHash,
    },
    DownloadCommit {
        output: OutputHash,
    },
}
