//! The real-IO host: executes the resolver's effects against filesystem
//! stores and subprocess builds, synchronously and in emission order. The
//! DAG definition layer does not exist yet, so callers hand over a
//! `RawInput` directly. Parallel builds / core scheduling (B11) are a later
//! milestone — this driver is about making the effect vocabulary real.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;

use xin_resolver::events::{DownloadOutcome, Effect, Event, Presence, PresenceAnswer};
use xin_resolver::hashes::OutputHash;
use xin_resolver::input::{BuilderType, RawInput, StoreName};
use xin_resolver::resolver::Outcome;
use xin_resolver::{Resolver, failure::BuildLog};

use crate::builder::{BuildRun, run_process_build};
use crate::local_store::LocalStore;

pub enum Backend {
    Local(LocalStore),
    /// the only remote we have so far: it never knows anything. Downloads
    /// can therefore never be demanded from it — the resolver only
    /// downloads from stores that claimed availability.
    DummyRemote,
}

struct Staged {
    out: PathBuf,
    build_dir: Option<PathBuf>,
}

pub struct Driver {
    /// definition order = the resolver's store ranking order
    stores: Vec<(StoreName, Backend)>,
    staged: BTreeMap<OutputHash, Staged>,
    pub builds_run: u32,
}

impl Driver {
    pub fn new(stores: Vec<(StoreName, Backend)>) -> Driver {
        Driver {
            stores,
            staged: BTreeMap::new(),
            builds_run: 0,
        }
    }

    fn backend_mut(&mut self, name: &StoreName) -> &mut Backend {
        self.stores
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b)
            .unwrap_or_else(|| panic!("effect for unknown store {name}"))
    }

    fn local_mut(&mut self, name: &StoreName) -> &mut LocalStore {
        match self.backend_mut(name) {
            Backend::Local(s) => s,
            Backend::DummyRemote => panic!("effect requiring a local store sent to remote {name}"),
        }
    }

    pub fn local(&self, name: &StoreName) -> &LocalStore {
        match self.stores.iter().find(|(n, _)| n == name).map(|(_, b)| b) {
            Some(Backend::Local(s)) => s,
            _ => panic!("no local store named {name}"),
        }
    }

    /// Where an output's bytes live, searching local stores in definition
    /// order (A4: multiple local stores are expected eventually).
    fn find_output(&self, oh: OutputHash) -> Option<PathBuf> {
        for (_, b) in &self.stores {
            if let Backend::Local(s) = b {
                let dir = s.output_dir(oh);
                if dir.is_dir() {
                    return Some(dir);
                }
            }
        }
        None
    }

    pub fn execute(&mut self, eff: Effect) -> io::Result<Option<Event>> {
        match eff {
            Effect::QueryMapping { tok, store, input } => {
                let output = match self.backend_mut(&store) {
                    Backend::Local(s) => s.lookup_mapping(input)?,
                    Backend::DummyRemote => None,
                };
                Ok(Some(Event::MappingAnswered { tok, output }))
            }
            Effect::QueryPresence { tok, store, output } => {
                let presence = match self.backend_mut(&store) {
                    Backend::Local(s) => s.presence(output)?,
                    Backend::DummyRemote => Presence::Missing,
                };
                Ok(Some(Event::PresenceAnswered {
                    tok,
                    answer: PresenceAnswer { output, presence },
                }))
            }
            Effect::AcquireLease {
                tok,
                store,
                protect,
            } => {
                let lease = self.local_mut(&store).acquire_lease(&protect)?;
                Ok(Some(Event::LeaseGranted { tok, lease }))
            }
            Effect::ReleaseLease { store, lease } => {
                self.local_mut(&store).release_lease(lease)?;
                Ok(None)
            }
            Effect::StartBuild {
                tok,
                input,
                builder,
                recipe,
                store,
                inputs,
                ..
            } => {
                self.builds_run += 1;
                let outcome = match builder {
                    BuilderType::Process => {
                        let found: BTreeMap<OutputHash, PathBuf> = inputs
                            .iter()
                            .filter_map(|(_, oh)| self.find_output(*oh).map(|p| (*oh, p)))
                            .collect();
                        let target = self.local_mut(&store);
                        let BuildRun {
                            outcome,
                            build_dir,
                            staged_out,
                        } = run_process_build(target, input, &recipe, &inputs, |oh| {
                            found.get(&oh).cloned()
                        })?;
                        if let xin_resolver::events::BuildOutcome::Success { output, .. } = &outcome
                        {
                            self.staged.insert(
                                *output,
                                Staged {
                                    out: staged_out.unwrap(),
                                    build_dir: Some(build_dir),
                                },
                            );
                        }
                        outcome
                    }
                    BuilderType::FetchUrl => xin_resolver::events::BuildOutcome::Failure {
                        log: BuildLog {
                            stdout: Vec::new(),
                            stderr: b"xin: FetchUrl builder not implemented in the driver yet"
                                .to_vec(),
                            return_code: -1,
                        },
                    },
                };
                // meta is fire-and-forget (§6): losing a log is harmless
                let log = match &outcome {
                    xin_resolver::events::BuildOutcome::Success { log, .. } => log.clone(),
                    xin_resolver::events::BuildOutcome::Failure { log } => log.clone(),
                };
                if let Err(e) = self.local_mut(&store).write_build_meta(input, &log) {
                    eprintln!("xin: could not write build meta for {input}: {e}");
                }
                Ok(Some(Event::BuildFinished { tok, outcome }))
            }
            Effect::StartDownload { tok, from, .. } => {
                // unreachable with the dummy remote (it never claims
                // availability); answer defensively instead of panicking
                Ok(Some(Event::DownloadFinished {
                    tok,
                    outcome: DownloadOutcome::Failure {
                        error: format!("store {from} cannot serve downloads"),
                    },
                }))
            }
            Effect::CommitOutput { tok, store, output } => {
                let staged = self
                    .staged
                    .remove(&output)
                    .unwrap_or_else(|| panic!("commit of unstaged output {output:?}"));
                let already_existed = self.local_mut(&store).commit_output(output, &staged.out)?;
                if let Some(build_dir) = staged.build_dir {
                    let _ = std::fs::remove_dir_all(build_dir); // leftover recipe/inputs scaffolding
                }
                Ok(Some(Event::OutputCommitted {
                    tok,
                    already_existed,
                }))
            }
            Effect::CommitMapping {
                tok,
                store,
                input,
                output,
            } => {
                let result = self.local_mut(&store).commit_mapping(input, output)?;
                Ok(Some(Event::MappingCommitted { tok, result }))
            }
        }
    }
}

/// Drive a resolver over real IO to quiescence (keep-going; fail-fast as a
/// host policy can reuse the same cancellation contract as `sim` later).
pub fn run_to_quiescence(resolver: &mut Resolver, driver: &mut Driver) -> io::Result<Outcome> {
    let mut pending: VecDeque<Effect> = resolver.start().into();
    while let Some(eff) = pending.pop_front() {
        if let Some(ev) = driver.execute(eff)? {
            pending.extend(resolver.apply(ev));
        }
    }
    Ok(resolver.quiesced())
}

/// Convenience: ingest, resolve, and run in one call.
pub fn resolve(raw: RawInput, mut driver: Driver) -> io::Result<(Resolver, Driver, Outcome)> {
    let mut resolver = Resolver::new(
        raw.ingest()
            .map_err(|e| io::Error::other(format!("{e:?}")))?,
    );
    let out = run_to_quiescence(&mut resolver, &mut driver)?;
    Ok((resolver, driver, out))
}
