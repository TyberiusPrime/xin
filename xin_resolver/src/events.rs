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

impl Effect {
    /// The correlation token, if the effect expects a completion event.
    pub fn tok(&self) -> Option<RequestId> {
        match self {
            Effect::QueryMapping { tok, .. }
            | Effect::QueryPresence { tok, .. }
            | Effect::AcquireLease { tok, .. }
            | Effect::StartBuild { tok, .. }
            | Effect::StartDownload { tok, .. }
            | Effect::CommitOutput { tok, .. }
            | Effect::CommitMapping { tok, .. } => Some(*tok),
            Effect::ReleaseLease { .. } => None,
        }
    }

    /// May a host drop this effect and send `Event::Cancelled` instead?
    /// Commits never: the store must not be left half-committed.
    pub fn cancellable(&self) -> bool {
        matches!(
            self,
            Effect::QueryMapping { .. }
                | Effect::QueryPresence { .. }
                | Effect::AcquireLease { .. }
                | Effect::StartBuild { .. }
                | Effect::StartDownload { .. }
        )
    }
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
    /// The host dropped or aborted the request (fail-fast drain). Only legal
    /// for cancellable effects — see `Effect::cancellable`.
    Cancelled {
        tok: RequestId,
    },
}

impl Event {
    pub fn tok(&self) -> RequestId {
        match self {
            Event::MappingAnswered { tok, .. }
            | Event::PresenceAnswered { tok, .. }
            | Event::LeaseGranted { tok, .. }
            | Event::BuildFinished { tok, .. }
            | Event::DownloadFinished { tok, .. }
            | Event::OutputCommitted { tok, .. }
            | Event::MappingCommitted { tok, .. }
            | Event::Cancelled { tok } => *tok,
        }
    }

    pub fn kind(&self) -> EventKind {
        match self {
            Event::MappingAnswered { .. } => EventKind::MappingAnswered,
            Event::PresenceAnswered { .. } => EventKind::PresenceAnswered,
            Event::LeaseGranted { .. } => EventKind::LeaseGranted,
            Event::BuildFinished { .. } => EventKind::BuildFinished,
            Event::DownloadFinished { .. } => EventKind::DownloadFinished,
            Event::OutputCommitted { .. } => EventKind::OutputCommitted,
            Event::MappingCommitted { .. } => EventKind::MappingCommitted,
            Event::Cancelled { .. } => EventKind::Cancelled,
        }
    }
}

/// A presence answer is self-describing: it echoes the queried output-hash
/// (sanity-checked against the token) and, when present, always carries the
/// declared runtime refs — local stores read them from the stored tree,
/// remotes report them as an availability *claim*, which lets the resolver
/// expand and pre-fetch the runtime closure before the download and feeds
/// the §8 validation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PresenceAnswer {
    pub output: OutputHash,
    pub presence: Presence,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Presence {
    Missing,
    Present { runtime_refs: BTreeSet<OutputHash> },
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
    /// A12: persisting a mapping learned from a remote into a local store
    MappingWriteBack {
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

impl Inflight {
    pub fn kind(&self) -> InflightKind {
        match self {
            Inflight::MappingQuery { .. } => InflightKind::MappingQuery,
            Inflight::PresenceQuery { .. } => InflightKind::PresenceQuery,
            Inflight::BuildLease { .. } => InflightKind::BuildLease,
            Inflight::Build { .. } => InflightKind::Build,
            Inflight::BuildOutputCommit { .. } => InflightKind::BuildOutputCommit,
            Inflight::BuildMappingCommit { .. } => InflightKind::BuildMappingCommit,
            Inflight::MappingWriteBack { .. } => InflightKind::MappingWriteBack,
            Inflight::DownloadLease { .. } => InflightKind::DownloadLease,
            Inflight::Download { .. } => InflightKind::Download,
            Inflight::DownloadCommit { .. } => InflightKind::DownloadCommit,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    MappingAnswered,
    PresenceAnswered,
    LeaseGranted,
    BuildFinished,
    DownloadFinished,
    OutputCommitted,
    MappingCommitted,
    Cancelled,
}

impl EventKind {
    pub const ALL: &[EventKind] = &[
        EventKind::MappingAnswered,
        EventKind::PresenceAnswered,
        EventKind::LeaseGranted,
        EventKind::BuildFinished,
        EventKind::DownloadFinished,
        EventKind::OutputCommitted,
        EventKind::MappingCommitted,
        EventKind::Cancelled,
    ];
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InflightKind {
    MappingQuery,
    PresenceQuery,
    BuildLease,
    Build,
    BuildOutputCommit,
    BuildMappingCommit,
    MappingWriteBack,
    DownloadLease,
    Download,
    DownloadCommit,
}

impl InflightKind {
    pub const ALL: &[InflightKind] = &[
        InflightKind::MappingQuery,
        InflightKind::PresenceQuery,
        InflightKind::BuildLease,
        InflightKind::Build,
        InflightKind::BuildOutputCommit,
        InflightKind::BuildMappingCommit,
        InflightKind::MappingWriteBack,
        InflightKind::DownloadLease,
        InflightKind::Download,
        InflightKind::DownloadCommit,
    ];
}
