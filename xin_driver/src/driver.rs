//! The real-IO host: executes the resolver's effects against filesystem
//! stores and subprocess builds, synchronously and in emission order.
//! Parallel builds / core scheduling (B11) are a later milestone — this
//! driver is about making the effect vocabulary real.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;

use xin_resolver::events::{
    BuildOutcome, DownloadOutcome, Effect, Event, Presence, PresenceAnswer,
};
use xin_resolver::hashes::OutputHash;
use xin_resolver::input::{BuilderType, RawInput, StoreName};
use xin_resolver::resolver::Outcome;
use xin_resolver::sim::Policy;
use xin_resolver::{Resolver, failure::BuildLog};

use crate::builder::{BuildRun, run_process_build};
use crate::container::ContainerMode;
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

/// The exit code the driver reports for builds it *refused to run* in
/// query-only mode; lets `xin status` tell "needs build" from real
/// failures. Out of the 0..=255 range a process can produce.
pub const QUERY_ONLY_EXIT: i32 = -75;

pub struct Driver {
    /// definition order = the resolver's store ranking order
    stores: Vec<(StoreName, Backend)>,
    staged: BTreeMap<OutputHash, Staged>,
    pub builds_run: u32,
    /// answer store queries truthfully but refuse builds and downloads
    /// (with `QUERY_ONLY_EXIT`); the basis of `xin status`
    pub query_only: bool,
    /// how Process builds execute; `Direct` unless the host opts in
    pub container: ContainerMode,
}

impl Driver {
    pub fn new(stores: Vec<(StoreName, Backend)>) -> Driver {
        Driver {
            stores,
            staged: BTreeMap::new(),
            builds_run: 0,
            query_only: false,
            container: ContainerMode::Direct,
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
                if self.query_only {
                    return Ok(Some(Event::BuildFinished {
                        tok,
                        outcome: BuildOutcome::Failure {
                            log: BuildLog {
                                stdout: Vec::new(),
                                stderr: b"xin: build not attempted (status query)".to_vec(),
                                return_code: QUERY_ONLY_EXIT,
                            },
                        },
                    }));
                }
                self.builds_run += 1;
                let outcome = match builder {
                    BuilderType::Process => {
                        let found: BTreeMap<OutputHash, PathBuf> = inputs
                            .iter()
                            .filter_map(|(_, oh)| self.find_output(*oh).map(|p| (*oh, p)))
                            .collect();
                        let mode = self.container.clone();
                        let target = self.local_mut(&store);
                        let BuildRun {
                            outcome,
                            build_dir,
                            staged_out,
                        } = run_process_build(
                            target,
                            input,
                            &recipe,
                            &inputs,
                            |oh| found.get(&oh).cloned(),
                            &mode,
                        )?;
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
                // query-only refuses; otherwise unreachable with the dummy
                // remote (it never claims availability) — answer
                // defensively instead of panicking
                let error = if self.query_only {
                    "xin: download not attempted (status query)".to_owned()
                } else {
                    format!("store {from} cannot serve downloads")
                };
                Ok(Some(Event::DownloadFinished {
                    tok,
                    outcome: DownloadOutcome::Failure { error },
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

/// Drive a resolver over real IO to quiescence. Fail-fast is the same host
/// policy as in `sim::step`: once the core has recorded a failure, pending
/// cancellable effects are answered with `Event::Cancelled` instead of
/// executed; commits and lease releases always run to completion (§7).
pub fn run_with_policy(
    resolver: &mut Resolver,
    driver: &mut Driver,
    policy: Policy,
) -> io::Result<Outcome> {
    let mut pending: VecDeque<Effect> = resolver.start().into();
    while let Some(eff) = pending.pop_front() {
        if policy == Policy::FailFast && !resolver.failures.is_empty() && eff.cancellable() {
            let tok = eff.tok().unwrap();
            pending.extend(resolver.apply(Event::Cancelled { tok }));
        } else if let Some(ev) = driver.execute(eff)? {
            pending.extend(resolver.apply(ev));
        }
    }
    Ok(resolver.quiesced())
}

pub fn run_to_quiescence(resolver: &mut Resolver, driver: &mut Driver) -> io::Result<Outcome> {
    run_with_policy(resolver, driver, Policy::KeepGoing)
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
