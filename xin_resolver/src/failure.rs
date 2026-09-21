//! One failure table, id-referenced (resolver-architecture.md §7).
//!
//! Nodes hold a `FailureId`; the causal chain is *reconstructed* by walking
//! `origin` at report time, never duplicated per node. Keep-going vs
//! fail-fast is a host-scheduler policy, not a second code path here.

use crate::hashes::OutputHash;
use crate::input::{NodeId, StoreName};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FailureId(pub u32);

impl FailureId {
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FailureRecord {
    pub kind: FailureKind,
    /// who to blame; for `Upstream` this names the (lowest-id) failed direct
    /// upstream — walk its own record for the rest of the chain
    pub origin: Origin,
    pub detail: FailureDetail,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Node(NodeId),
    /// runtime-closure expansion can fail on outputs we never named (A7)
    Output(OutputHash),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureKind {
    Build,
    Download,
    /// A8: stores disagree on an input→output mapping
    MappingConflict,
    /// B5: commit found the same input-hash mapped to a *different* output
    /// on disk, or a rebuild reproduced different bytes
    NonDeterminism,
    /// A2: a fetch reproduced different bytes than the recorded TOFU hash
    TofuMismatch,
    /// §8: a substituted output's runtime refs escape the build closure
    /// (validation lands in M4; the variant is part of the taxonomy now)
    ClosureEscape,
    /// an output is demanded, no store has it, and no producer is known
    MissingRuntime,
    Upstream,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum FailureDetail {
    #[default]
    None,
    BuildLog(BuildLog),
    Text(String),
    ConflictingAnswers(Vec<(StoreName, OutputHash)>),
}

#[derive(Clone, PartialEq, Eq)]
pub struct BuildLog {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub return_code: i32,
}

impl std::fmt::Debug for BuildLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildLog")
            .field("stdout", &String::from_utf8_lossy(&self.stdout))
            .field("stderr", &String::from_utf8_lossy(&self.stderr))
            .field("return_code", &self.return_code)
            .finish()
    }
}
