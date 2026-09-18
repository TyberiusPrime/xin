//! Structured trace layer. The core stays pure: tracing appends entries to
//! an in-memory buffer (opt-in, off by default so state clones stay cheap
//! in the DST explorers); rendering — with human node names — happens on
//! demand via `Resolver::trace_report()` or `render()`. A host that wants
//! live logging drains the buffer after each `apply` and prints.

use std::fmt::Write as _;

use crate::events::{BuildOutcome, DownloadOutcome, Effect, Event};
use crate::failure::{FailureId, FailureKind, Origin};
use crate::hashes::{InputHash, OutputHash};
use crate::input::{Dag, NodeId};
use crate::state::MappingSource;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TraceEntry {
    /// an event entered `apply`
    Apply(Event),
    /// an effect left the core
    Emit(Effect),
    /// a node's input-hash was computed
    NodeNamed { node: NodeId, input: InputHash },
    /// a mapping fact was learned (query wave, or our own build)
    MappingResolved {
        input: InputHash,
        output: OutputHash,
        source: MappingSource,
    },
    FailureRecorded {
        id: FailureId,
        kind: FailureKind,
        origin: Origin,
    },
    /// an output's whole runtime closure became locally present (may repeat
    /// when a later refs verification re-confirms completion)
    Realized { output: OutputHash },
}

#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub enabled: bool,
    pub entries: Vec<TraceEntry>,
}

impl Trace {
    pub fn push(&mut self, e: TraceEntry) {
        if self.enabled {
            self.entries.push(e);
        }
    }

    /// Take everything recorded so far (for hosts that log live).
    pub fn drain(&mut self) -> Vec<TraceEntry> {
        std::mem::take(&mut self.entries)
    }
}

fn fmt_event(ev: &Event) -> String {
    match ev {
        Event::BuildFinished {
            tok,
            outcome:
                BuildOutcome::Success {
                    output,
                    runtime_refs,
                    ..
                },
        } => {
            format!(
                "BuildFinished {tok:?} ok {output:?} refs={}",
                runtime_refs.len()
            )
        }
        Event::BuildFinished {
            tok,
            outcome: BuildOutcome::Failure { .. },
        } => {
            format!("BuildFinished {tok:?} FAILED")
        }
        Event::DownloadFinished {
            tok,
            outcome: DownloadOutcome::Success { runtime_refs },
        } => {
            format!("DownloadFinished {tok:?} ok refs={}", runtime_refs.len())
        }
        other => format!("{other:?}"),
    }
}

fn fmt_effect(dag: &Dag, eff: &Effect) -> String {
    match eff {
        Effect::StartBuild {
            tok,
            node,
            input,
            builder,
            store,
            inputs,
            ..
        } => format!(
            "StartBuild {tok:?} node {} ({input:?}, {builder:?}, {} inputs) in {store}",
            dag.nodes[node.idx()].name,
            inputs.len()
        ),
        other => format!("{other:?}"),
    }
}

/// Render entries as one line each, resolving node ids to human names.
pub fn render(dag: &Dag, entries: &[TraceEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        let _ = match e {
            TraceEntry::Apply(ev) => writeln!(out, "<- {}", fmt_event(ev)),
            TraceEntry::Emit(eff) => writeln!(out, "-> {}", fmt_effect(dag, eff)),
            TraceEntry::NodeNamed { node, input } => {
                writeln!(out, "   named {} = {input:?}", dag.nodes[node.idx()].name)
            }
            TraceEntry::MappingResolved {
                input,
                output,
                source,
            } => {
                writeln!(out, "   resolved {input:?} -> {output:?} via {source:?}")
            }
            TraceEntry::FailureRecorded { id, kind, origin } => {
                let origin = match origin {
                    Origin::Node(n) => format!("node {}", dag.nodes[n.idx()].name),
                    Origin::Output(oh) => format!("{oh:?}"),
                };
                writeln!(out, "   FAILURE {id:?} {kind:?} at {origin}")
            }
            TraceEntry::Realized { output } => writeln!(out, "   realized {output:?}"),
        };
    }
    out
}
