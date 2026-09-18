//! xin_resolver — the event-driven resolver (resolver-plan.md, built to
//! resolver-architecture.md).
//!
//! Shape: a pure sync core (`Resolver::start` / `apply` / `quiesced`) —
//! events in, effects out, no IO/clock/RNG anywhere near it — with state
//! keyed three ways: `NodeId` (demand, blame, human-facing status),
//! `InputHash` (naming, building), `OutputHash` (presence, downloading).
//! `sim` is the deterministic host: one world mock plus a seeded scheduler.
//!
//! Milestone status: M1–M3. The transition table (`transitions`) classifies
//! every (request × event) pair and pins each request to its legal states;
//! fail-fast is a host policy built on `Event::Cancelled`; §8 closure-escape
//! validation runs on substituted outputs whenever the build closure is
//! conclusively known; A12 write-back persists remotely-learned mappings.
//! Negative caching is the in-process asked-once sets — TTLs only matter
//! once knowledge is persisted to disk (B5), which is when they land.
//! Not here yet (M4–M6): per-remote blacklists, per-store TOFU trust
//! levels, GC-vs-lease simulation, exhaustive small-DAG interleaving
//! enumeration, proptest DAG generation.

pub mod events;
pub mod failure;
pub mod hashes;
pub mod input;
pub mod resolver;
pub mod sim;
pub mod state;
pub mod transitions;

pub use resolver::{NodeStatus, Outcome, Resolver};
