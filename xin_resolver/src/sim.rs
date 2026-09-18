//! Deterministic simulation host (resolver-architecture.md §2, §9): one
//! `SimWorld` owning stores and builder behavior — not per-trait mocks —
//! plus a seeded scheduler that picks which pending effect executes next.
//! Effects mutate the world at *delivery* time, so the schedule genuinely
//! reorders completions. No runtime, no madsim/turmoil needed yet.

use std::collections::{BTreeMap, BTreeSet};

use crate::events::{
    BuildOutcome, CommitResult, DownloadOutcome, Effect, Event, LeaseId, Presence, PresenceAnswer,
};
use crate::failure::BuildLog;
use crate::hashes::{InputHash, OutputHash, hash_bytes};
use crate::input::{Dag, NodeId, StoreDef, StoreName};
use crate::resolver::{Outcome, Resolver};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SimStore {
    pub mappings: BTreeMap<InputHash, OutputHash>,
    /// output-hash → declared runtime refs
    pub outputs: BTreeMap<OutputHash, BTreeSet<OutputHash>>,
    /// what the store *claims* as runtime refs in presence answers, when it
    /// differs from the truth — lets tests script lying/corrupt stores
    pub claim_overrides: BTreeMap<OutputHash, BTreeSet<OutputHash>>,
}

/// Scripted behavior for one node's builds (failure injection).
#[derive(Clone, Debug)]
pub enum BuildScript {
    /// output derived from the input-hash; runtime refs are the inputs whose
    /// alias the recipe lists as `rt:alias1,alias2`
    Default,
    Fail {
        stderr: String,
    },
    /// produce exactly this — for scripting TOFU / nondeterminism cases
    Produce {
        output: OutputHash,
        runtime_refs: BTreeSet<OutputHash>,
    },
}

#[derive(Debug, Default)]
pub struct SimWorld {
    pub stores: BTreeMap<StoreName, (StoreDef, SimStore)>,
    pub builds: BTreeMap<NodeId, BuildScript>,
    pub fail_downloads: BTreeSet<OutputHash>,
    /// finished-but-uncommitted trees (the store's temp dir): oh → refs
    pub staged: BTreeMap<OutputHash, BTreeSet<OutputHash>>,
    pub leases: BTreeMap<u64, BTreeSet<OutputHash>>,
    pub next_lease: u64,
    pub builds_run: u32,
    pub downloads_run: u32,
}

/// The deterministic sim output for a default-scripted build.
pub fn sim_output_for(ih: InputHash) -> OutputHash {
    let mut pre = b"sim-built:".to_vec();
    pre.extend_from_slice(&ih.0);
    OutputHash(hash_bytes(&pre))
}

fn ok_log() -> BuildLog {
    BuildLog {
        stdout: b"ok".to_vec(),
        stderr: Vec::new(),
        return_code: 0,
    }
}

fn recipe_runtime_aliases(recipe: &[u8]) -> Vec<&str> {
    let Ok(s) = std::str::from_utf8(recipe) else {
        return Vec::new();
    };
    let Some(rest) = s.strip_prefix("rt:") else {
        return Vec::new();
    };
    rest.split(',').filter(|a| !a.is_empty()).collect()
}

impl SimWorld {
    pub fn new(dag: &Dag) -> SimWorld {
        SimWorld {
            stores: dag
                .stores
                .iter()
                .map(|(n, d)| (n.clone(), (d.clone(), SimStore::default())))
                .collect(),
            ..SimWorld::default()
        }
    }

    pub fn store(&self, name: &StoreName) -> &SimStore {
        &self.stores[name].1
    }

    pub fn store_mut(&mut self, name: &StoreName) -> &mut SimStore {
        &mut self.stores.get_mut(name).unwrap().1
    }

    /// Execute one effect against the world; `None` for fire-and-forget
    /// effects that produce no completion event.
    pub fn execute(&mut self, eff: Effect) -> Option<Event> {
        match eff {
            Effect::QueryMapping { tok, store, input } => {
                let output = self.store(&store).mappings.get(&input).copied();
                Some(Event::MappingAnswered { tok, output })
            }
            Effect::QueryPresence { tok, store, output } => {
                let s = self.store(&store);
                let presence = match s.outputs.get(&output) {
                    None => Presence::Missing,
                    Some(refs) => {
                        let runtime_refs = s
                            .claim_overrides
                            .get(&output)
                            .cloned()
                            .unwrap_or_else(|| refs.clone());
                        Presence::Present { runtime_refs }
                    }
                };
                Some(Event::PresenceAnswered {
                    tok,
                    answer: PresenceAnswer { output, presence },
                })
            }
            Effect::AcquireLease {
                tok,
                store: _,
                protect,
            } => {
                self.next_lease += 1;
                self.leases.insert(self.next_lease, protect);
                Some(Event::LeaseGranted {
                    tok,
                    lease: LeaseId(self.next_lease),
                })
            }
            Effect::ReleaseLease { lease, .. } => {
                assert!(
                    self.leases.remove(&lease.0).is_some(),
                    "released unknown lease"
                );
                None
            }
            Effect::StartBuild {
                tok,
                node,
                input,
                recipe,
                inputs,
                ..
            } => {
                self.builds_run += 1;
                let script = self
                    .builds
                    .get(&node)
                    .cloned()
                    .unwrap_or(BuildScript::Default);
                let outcome = match script {
                    BuildScript::Default => {
                        let output = sim_output_for(input);
                        let aliases = recipe_runtime_aliases(&recipe);
                        let runtime_refs: BTreeSet<OutputHash> = inputs
                            .iter()
                            .filter(|(a, _)| aliases.contains(&a.as_str()))
                            .map(|(_, oh)| *oh)
                            .collect();
                        self.staged.insert(output, runtime_refs.clone());
                        BuildOutcome::Success {
                            output,
                            runtime_refs,
                            log: ok_log(),
                        }
                    }
                    BuildScript::Fail { stderr } => BuildOutcome::Failure {
                        log: BuildLog {
                            stdout: Vec::new(),
                            stderr: stderr.into_bytes(),
                            return_code: 1,
                        },
                    },
                    BuildScript::Produce {
                        output,
                        runtime_refs,
                    } => {
                        self.staged.insert(output, runtime_refs.clone());
                        BuildOutcome::Success {
                            output,
                            runtime_refs,
                            log: ok_log(),
                        }
                    }
                };
                Some(Event::BuildFinished { tok, outcome })
            }
            Effect::StartDownload {
                tok, output, from, ..
            } => {
                self.downloads_run += 1;
                let outcome = if self.fail_downloads.contains(&output) {
                    DownloadOutcome::Failure {
                        error: format!("simulated download failure from {from}"),
                    }
                } else {
                    let refs = self
                        .store(&from)
                        .outputs
                        .get(&output)
                        .cloned()
                        .expect("bug in sim scenario: download of unavailable output");
                    self.staged.insert(output, refs.clone());
                    DownloadOutcome::Success { runtime_refs: refs }
                };
                Some(Event::DownloadFinished { tok, outcome })
            }
            Effect::CommitOutput { tok, store, output } => {
                let refs = self
                    .staged
                    .get(&output)
                    .cloned()
                    .expect("commit of unstaged output");
                let s = self.store_mut(&store);
                let already_existed = s.outputs.insert(output, refs).is_some();
                Some(Event::OutputCommitted {
                    tok,
                    already_existed,
                })
            }
            Effect::CommitMapping {
                tok,
                store,
                input,
                output,
            } => {
                let s = self.store_mut(&store);
                let result = match s.mappings.get(&input) {
                    Some(existing) if *existing != output => CommitResult::Conflict {
                        existing: *existing,
                    },
                    _ => {
                        s.mappings.insert(input, output);
                        CommitResult::Committed
                    }
                };
                Some(Event::MappingCommitted { tok, result })
            }
        }
    }
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Drive to quiescence under a seeded schedule. Different seeds exercise
/// different interleavings; the confluence property says the end conditions
/// must not care.
pub fn drive(resolver: &mut Resolver, world: &mut SimWorld, seed: u64) -> Outcome {
    drive_policy(resolver, world, seed, Policy::KeepGoing)
}

/// §7: keep-going vs fail-fast is a *scheduler* policy, not a core code
/// path. FailFast stops dispatching new work once the core has recorded a
/// failure and drains the rest: cancellable effects are answered with
/// `Event::Cancelled`, commits and releases always run to completion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    KeepGoing,
    FailFast,
}

pub fn drive_policy(
    resolver: &mut Resolver,
    world: &mut SimWorld,
    seed: u64,
    policy: Policy,
) -> Outcome {
    let mut rng = seed.wrapping_mul(2685821657736338717).wrapping_add(1);
    let mut pending: Vec<Effect> = resolver.start();
    while !pending.is_empty() {
        let i = (xorshift(&mut rng) % pending.len() as u64) as usize;
        let eff = pending.swap_remove(i);
        if policy == Policy::FailFast && !resolver.failures.is_empty() && eff.cancellable() {
            let tok = eff.tok().unwrap();
            pending.extend(resolver.apply(Event::Cancelled { tok }));
        } else if let Some(ev) = world.execute(eff) {
            pending.extend(resolver.apply(ev));
        }
    }
    resolver.quiesced()
}
