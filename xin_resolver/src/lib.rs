//! xin_resolver — the event-driven resolver (resolver-plan.md, built to
//! resolver-architecture.md).
//!
//! Shape: a pure sync core (`Resolver::start` / `apply` / `quiesced`) —
//! events in, effects out, no IO/clock/RNG anywhere near it — with state
//! keyed three ways: `NodeId` (demand, blame, human-facing status),
//! `InputHash` (naming, building), `OutputHash` (presence, downloading).
//! `sim` is the deterministic host: one world mock plus a seeded scheduler.
//!
//! Milestone status: M1/M2 plus first failure paths and a seed-swept
//! confluence test. Not here yet (M3–M6): the exhaustive (state × event)
//! transition-table harness, per-remote blacklists and negative-cache TTLs
//! (needs timer events), §8 closure-escape validation on *substituted*
//! outputs, A12 mapping write-back after downloads, GC-vs-lease simulation,
//! exhaustive small-DAG interleaving enumeration, proptest DAG generation.

pub mod events;
pub mod failure;
pub mod hashes;
pub mod input;
pub mod resolver;
pub mod sim;
pub mod state;

pub use resolver::{NodeStatus, Outcome, Resolver};
