//! The pure core (resolver-architecture.md §2): a sync fold over events.
//! `start()` and `apply()` take no IO, no clock, no RNG; everything IO-ish
//! leaves as an `Effect` and comes back as an `Event`. Determinism hygiene
//! (§4): no HashMap iteration, all tie-breaks by stable rank (NodeId /
//! store definition order), decisions only on complete answer waves.
//!
//! A `(state, event)` pair the core does not expect is a bug and panics
//! with a message naming both — the M3 transition-table harness will
//! enumerate these systematically.

use std::collections::{BTreeMap, BTreeSet};
use std::mem;

use crate::events::{
    BuildOutcome, CommitResult, DownloadOutcome, Effect, Event, Inflight, LeaseId, PresenceAnswer,
    RequestId,
};
use crate::failure::{FailureDetail, FailureId, FailureKind, FailureRecord, Origin};
use crate::hashes::{InputHash, OutputHash, input_hash_of};
use crate::input::{BuilderType, Dag, NodeId, StoreDef, StoreName};
use crate::state::{
    BuildPhase, Demand, DownloadPhase, Mapping, MappingSource, MappingState, NodeSlot, Realization,
    RealizationState, Reason, StoreKnowledge,
};

pub struct Resolver {
    pub dag: Dag,
    pub nodes: Vec<NodeSlot>,
    pub mappings: BTreeMap<InputHash, Mapping>,
    pub realizations: BTreeMap<OutputHash, Realization>,
    pub knowledge: StoreKnowledge,
    pub inflight: BTreeMap<RequestId, Inflight>,
    pub failures: Vec<FailureRecord>,
    next_tok: u64,
}

/// Human-facing per-node end state (A7's end conditions).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NodeStatus {
    Realized {
        output: OutputHash,
        store: StoreName,
    },
    /// output-named; presence was never demanded (early cutoff, A1)
    Named {
        output: OutputHash,
    },
    Failed {
        failure: FailureId,
    },
}

#[derive(Clone, Debug)]
pub struct Outcome {
    /// indexed by NodeId
    pub statuses: Vec<NodeStatus>,
    pub failures: Vec<FailureRecord>,
    /// every target realized
    pub success: bool,
}

impl Outcome {
    /// Reconstruct a causal chain by walking `Upstream` origins (§7).
    pub fn chain(&self, fid: FailureId) -> Vec<FailureId> {
        let mut out = vec![fid];
        let mut cur = fid;
        loop {
            let rec = &self.failures[cur.idx()];
            if rec.kind != FailureKind::Upstream {
                break;
            }
            let Origin::Node(up) = rec.origin else { break };
            let NodeStatus::Failed { failure } = &self.statuses[up.idx()] else {
                break;
            };
            cur = *failure;
            out.push(cur);
        }
        out
    }
}

enum RealizeMove {
    Wait,
    Download {
        from: StoreName,
        to: StoreName,
    },
    Build {
        producer: InputHash,
        known: Option<OutputHash>,
    },
    ProducerFailed(FailureId),
    DeadEnd,
}

impl Resolver {
    pub fn new(dag: Dag) -> Resolver {
        let nodes = dag
            .nodes
            .iter()
            .map(|n| NodeSlot {
                input_hash: None,
                unnamed_upstreams: n.distinct_upstreams().len() as u32,
                demand: Demand {
                    name: true,
                    realize: BTreeSet::new(),
                },
                failure: None,
            })
            .collect();
        Resolver {
            dag,
            nodes,
            mappings: BTreeMap::new(),
            realizations: BTreeMap::new(),
            knowledge: StoreKnowledge::default(),
            inflight: BTreeMap::new(),
            failures: Vec::new(),
            next_tok: 0,
        }
    }

    /// Seed demand and name every zero-input node.
    pub fn start(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        for t in self.dag.targets.clone() {
            self.nodes[t.idx()].demand.realize.insert(Reason::Target(t));
        }
        for i in 0..self.dag.nodes.len() {
            if self.nodes[i].unnamed_upstreams == 0 {
                self.name_node(NodeId(i as u32), &mut fx);
            }
        }
        fx
    }

    /// Total over everything a well-behaved host can send. No IO, no clock.
    pub fn apply(&mut self, ev: Event) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            Event::MappingAnswered { tok, output } => {
                self.on_mapping_answered(tok, output, &mut fx)
            }
            Event::PresenceAnswered { tok, answer } => {
                self.on_presence_answered(tok, answer, &mut fx)
            }
            Event::LeaseGranted { tok, lease } => self.on_lease_granted(tok, lease, &mut fx),
            Event::BuildFinished { tok, outcome } => self.on_build_finished(tok, outcome, &mut fx),
            Event::DownloadFinished { tok, outcome } => {
                self.on_download_finished(tok, outcome, &mut fx)
            }
            Event::OutputCommitted { tok, .. } => self.on_output_committed(tok, &mut fx),
            Event::MappingCommitted { tok, result } => {
                self.on_mapping_committed(tok, result, &mut fx)
            }
        }
        fx
    }

    // ------------------------------------------------------------------
    // naming (the InputHash machine)

    fn name_node(&mut self, id: NodeId, fx: &mut Vec<Effect>) {
        debug_assert!(
            self.nodes[id.idx()].input_hash.is_none(),
            "node named twice"
        );
        let ih = {
            let dn = &self.dag.nodes[id.idx()];
            let mut inputs: Vec<(&str, OutputHash)> = Vec::with_capacity(dn.upstreams.len());
            for (alias, up) in &dn.upstreams {
                let oh = self
                    .node_output(*up)
                    .expect("unnamed_upstreams==0 but upstream unresolved");
                inputs.push((alias.as_str(), oh));
            }
            input_hash_of(&inputs, &dn.recipe)
        };
        self.nodes[id.idx()].input_hash = Some(ih);

        if let Some(m) = self.mappings.get_mut(&ih) {
            // A7 DAG collapse: a byte-identical recipe with identical inputs
            // exists elsewhere in the graph; join it, never build twice
            m.nodes.push(id);
            m.nodes.sort();
            match m.state.clone() {
                MappingState::Resolved { output } => self.on_node_named(id, output, fx),
                MappingState::Building {
                    known_output: Some(output),
                    ..
                } => self.on_node_named(id, output, fx),
                MappingState::Failed(fid) => {
                    if self.nodes[id.idx()].failure.is_none() {
                        self.nodes[id.idx()].failure = Some(fid);
                    }
                }
                _ => {} // resolution in progress; we are on the list
            }
        } else {
            self.mappings.insert(
                ih,
                Mapping {
                    state: MappingState::Unresolved,
                    nodes: vec![id],
                },
            );
            self.begin_query(ih, fx);
        }
    }

    fn begin_query(&mut self, ih: InputHash, fx: &mut Vec<Effect>) {
        let node_list = self.mappings[&ih].nodes.clone();
        let mut pending: BTreeSet<StoreName> = BTreeSet::new();
        for (sname, sdef) in self.dag.stores.iter() {
            let ask = match sdef {
                StoreDef::Local { .. } => true,
                StoreDef::Remote => node_list
                    .iter()
                    .any(|n| self.dag.nodes[n.idx()].remotes.allows(sname)),
            };
            if ask && !self.knowledge.asked_mappings.contains(&(sname.clone(), ih)) {
                pending.insert(sname.clone());
            }
        }
        for s in &pending {
            self.knowledge.asked_mappings.insert((s.clone(), ih));
        }
        if pending.is_empty() {
            self.start_build_path(ih, None, fx);
            return;
        }
        for s in pending.clone() {
            let tok = self.tok(Inflight::MappingQuery {
                store: s.clone(),
                input: ih,
            });
            fx.push(Effect::QueryMapping {
                tok,
                store: s,
                input: ih,
            });
        }
        self.mappings.get_mut(&ih).unwrap().state = MappingState::Querying {
            pending,
            answers: Vec::new(),
        };
    }

    fn on_mapping_answered(
        &mut self,
        tok: RequestId,
        output: Option<OutputHash>,
        fx: &mut Vec<Effect>,
    ) {
        let Inflight::MappingQuery { store, input: ih } = self.take_inflight(tok) else {
            panic!("bug: MappingAnswered for a non-mapping-query token");
        };
        {
            let m = self.mappings.get_mut(&ih).unwrap();
            let MappingState::Querying { pending, answers } = &mut m.state else {
                panic!("bug: mapping answer in state {:?}", m.state);
            };
            assert!(pending.remove(&store), "bug: duplicate answer from {store}");
            if let Some(oh) = output {
                answers.push((store, oh));
            }
            if !pending.is_empty() {
                return; // decide only on the complete wave
            }
        }
        let answers = {
            let MappingState::Querying { answers, .. } = &self.mappings[&ih].state else {
                unreachable!()
            };
            answers.clone()
        };
        if answers.is_empty() {
            // nobody knows: build to learn the name (the backward spread)
            self.start_build_path(ih, None, fx);
            return;
        }
        let distinct: BTreeSet<OutputHash> = answers.iter().map(|(_, o)| *o).collect();
        if distinct.len() > 1 {
            // A8: conflicting information from stores is a build failure for
            // the node; per-store blacklisting comes with M4
            let blame = self.mappings[&ih].nodes[0];
            let fid = self.record_failure(
                FailureKind::MappingConflict,
                Origin::Node(blame),
                FailureDetail::ConflictingAnswers(answers),
            );
            self.fail_mapping(ih, fid, fx);
            return;
        }
        let oh = *distinct.iter().next().unwrap();
        self.resolve_mapping(ih, oh, MappingSource::Substituted, fx);
    }

    fn resolve_mapping(
        &mut self,
        ih: InputHash,
        oh: OutputHash,
        src: MappingSource,
        fx: &mut Vec<Effect>,
    ) {
        let prev = self.knowledge.facts.insert(ih, (oh, src));
        assert!(
            prev.is_none_or(|(p, _)| p == oh),
            "bug: fact overwrite for {ih:?} — nondeterminism must fail the node instead"
        );
        self.mappings.get_mut(&ih).unwrap().state = MappingState::Resolved { output: oh };
        let nodes = self.mappings[&ih].nodes.clone();
        for n in nodes {
            self.on_node_named(n, oh, fx);
        }
    }

    /// A node's output name became known: register the realization, transfer
    /// accumulated realize-demand to it, cascade naming downstream.
    fn on_node_named(&mut self, n: NodeId, oh: OutputHash, fx: &mut Vec<Effect>) {
        let ih = self.nodes[n.idx()].input_hash;
        {
            let r = self.realizations.entry(oh).or_default();
            if r.producer.is_none() {
                r.producer = ih;
            }
        }
        let reasons: Vec<Reason> = self.nodes[n.idx()].demand.realize.iter().copied().collect();
        for reason in reasons {
            self.demand_output(oh, reason, fx);
        }
        let downs = self.dag.nodes[n.idx()].downstreams.clone();
        for d in downs {
            let slot = &mut self.nodes[d.idx()];
            slot.unnamed_upstreams -= 1;
            if slot.unnamed_upstreams == 0 {
                self.name_node(d, fx);
            }
        }
    }

    // ------------------------------------------------------------------
    // demand (monotone; never retracted)

    /// Demand that a node's bytes (and runtime closure) end up local.
    fn demand_node(&mut self, n: NodeId, reason: Reason, fx: &mut Vec<Effect>) {
        if !self.nodes[n.idx()].demand.realize.insert(reason) {
            return;
        }
        if let Some(oh) = self.node_output(n) {
            self.demand_output(oh, reason, fx);
        }
        // not yet named: the demand transfers when the name arrives
    }

    fn demand_output(&mut self, oh: OutputHash, reason: Reason, fx: &mut Vec<Effect>) {
        {
            let r = self.realizations.entry(oh).or_default();
            if !r.demand.insert(reason) {
                return;
            }
        }
        if self.fully_realized(oh) {
            self.notify_reason(oh, reason, fx);
            return;
        }
        let need_query = {
            let r = &self.realizations[&oh];
            matches!(r.state, RealizationState::Absent) && !r.queried
        };
        if need_query {
            self.query_presence(oh, fx);
        }
        self.advance_realization(oh, fx);
    }

    // ------------------------------------------------------------------
    // realization (the OutputHash machine)

    fn query_presence(&mut self, oh: OutputHash, fx: &mut Vec<Effect>) {
        // every local store; remotes allowed by the producing node's policy.
        // Orphan runtime outputs (no producer known) ask all remotes —
        // simplification until per-store trust lands in M4.
        let producer_nodes: Vec<NodeId> = match self.realizations[&oh].producer {
            Some(pih) => self.mappings[&pih].nodes.clone(),
            None => Vec::new(),
        };
        let mut pending: BTreeSet<StoreName> = BTreeSet::new();
        for (sname, sdef) in self.dag.stores.iter() {
            let ask = match sdef {
                StoreDef::Local { .. } => true,
                StoreDef::Remote => {
                    producer_nodes.is_empty()
                        || producer_nodes
                            .iter()
                            .any(|n| self.dag.nodes[n.idx()].remotes.allows(sname))
                }
            };
            if ask && !self.knowledge.asked_presence.contains(&(sname.clone(), oh)) {
                pending.insert(sname.clone());
            }
        }
        for s in &pending {
            self.knowledge.asked_presence.insert((s.clone(), oh));
        }
        for s in pending.clone() {
            let tok = self.tok(Inflight::PresenceQuery {
                store: s.clone(),
                output: oh,
            });
            fx.push(Effect::QueryPresence {
                tok,
                store: s,
                output: oh,
            });
        }
        let r = self.realizations.get_mut(&oh).unwrap();
        r.queried = true;
        r.pending_presence = pending;
    }

    fn on_presence_answered(
        &mut self,
        tok: RequestId,
        answer: PresenceAnswer,
        fx: &mut Vec<Effect>,
    ) {
        let Inflight::PresenceQuery { store, output: oh } = self.take_inflight(tok) else {
            panic!("bug: PresenceAnswered for a non-presence-query token");
        };
        let is_local = matches!(self.dag.stores[&store], StoreDef::Local { .. });
        let mut learned_refs: Option<BTreeSet<OutputHash>> = None;
        {
            let r = self.realizations.get_mut(&oh).unwrap();
            r.pending_presence.remove(&store);
            if answer.present {
                if is_local {
                    r.present_in.insert(store.clone());
                    if matches!(r.state, RealizationState::Absent) {
                        r.state = RealizationState::Present {
                            store: store.clone(),
                        };
                    }
                    if r.rt_refs.is_none() {
                        learned_refs = Some(answer.runtime_refs.unwrap_or_else(|| {
                            panic!(
                                "bug: local store {store} reported presence without runtime refs"
                            )
                        }));
                    }
                } else {
                    r.available_in.insert(store);
                }
            }
        }
        if let Some(refs) = learned_refs {
            self.learn_runtime_refs(oh, refs, fx);
        }
        self.advance_realization(oh, fx);
    }

    /// The declared runtime refs of an output became known. This is where
    /// A7's "scope of targets expands": refs are bare output-hashes and may
    /// belong to nodes we never named.
    fn learn_runtime_refs(
        &mut self,
        oh: OutputHash,
        refs: BTreeSet<OutputHash>,
        fx: &mut Vec<Effect>,
    ) {
        {
            let r = self.realizations.get_mut(&oh).unwrap();
            if r.rt_refs.is_some() {
                return; // a fact, inserted once
            }
            r.rt_refs = Some(refs.clone());
        }
        for dep in refs {
            if self.fully_realized(dep) {
                continue;
            }
            self.realizations
                .get_mut(&oh)
                .unwrap()
                .rt_missing
                .insert(dep);
            self.demand_output(dep, Reason::RuntimeOf(oh), fx);
        }
        if self.fully_realized(oh) {
            self.on_realized(oh, fx);
        }
    }

    /// Decide the next move for a demanded, absent output. Only ever acts
    /// once the full presence wave is in (determinism).
    fn advance_realization(&mut self, oh: OutputHash, fx: &mut Vec<Effect>) {
        let mv = {
            let r = &self.realizations[&oh];
            if r.demand.is_empty()
                || !matches!(r.state, RealizationState::Absent)
                || !r.pending_presence.is_empty()
            {
                RealizeMove::Wait
            } else if let Some(from) = r
                .available_in
                .iter()
                .min_by_key(|s| self.store_rank(s))
                .cloned()
            {
                RealizeMove::Download {
                    from,
                    to: self.download_target_store(oh),
                }
            } else if let Some(pih) = r.producer {
                match &self.mappings[&pih].state {
                    MappingState::Failed(fid) => RealizeMove::ProducerFailed(*fid),
                    _ => RealizeMove::Build {
                        producer: pih,
                        known: self.mapping_output(pih),
                    },
                }
            } else {
                RealizeMove::DeadEnd
            }
        };
        match mv {
            RealizeMove::Wait => {}
            RealizeMove::Download { from, to } => {
                let tok = self.tok(Inflight::DownloadLease { output: oh });
                self.realizations.get_mut(&oh).unwrap().state = RealizationState::Downloading {
                    from,
                    to: to.clone(),
                    lease: None,
                    phase: DownloadPhase::AwaitingLease,
                };
                fx.push(Effect::AcquireLease {
                    tok,
                    store: to,
                    protect: [oh].into_iter().collect(),
                });
            }
            RealizeMove::Build { producer, known } => self.start_build_path(producer, known, fx),
            RealizeMove::ProducerFailed(fid) => self.fail_realization(oh, fid, fx),
            RealizeMove::DeadEnd => {
                let fid = self.record_failure(
                    FailureKind::MissingRuntime,
                    Origin::Output(oh),
                    FailureDetail::None,
                );
                self.fail_realization(oh, fid, fx);
            }
        }
    }

    /// An output's whole runtime closure is now locally present: notify
    /// everything that was waiting.
    fn on_realized(&mut self, oh: OutputHash, fx: &mut Vec<Effect>) {
        let reasons: Vec<Reason> = self.realizations[&oh].demand.iter().copied().collect();
        for reason in reasons {
            self.notify_reason(oh, reason, fx);
        }
    }

    fn notify_reason(&mut self, oh: OutputHash, reason: Reason, fx: &mut Vec<Effect>) {
        match reason {
            Reason::Target(_) => {} // read off at report time
            Reason::BuildInputOf(builder) => self.try_start_build(builder, fx),
            Reason::RuntimeOf(parent) => {
                let removed = match self.realizations.get_mut(&parent) {
                    Some(r) => r.rt_missing.remove(&oh),
                    None => false,
                };
                if removed && self.fully_realized(parent) {
                    self.on_realized(parent, fx);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // building

    /// Enter the build path for a mapping — either to learn its name (no
    /// store knew it) or to produce named-but-absent bytes.
    fn start_build_path(&mut self, ih: InputHash, known: Option<OutputHash>, fx: &mut Vec<Effect>) {
        {
            let m = self.mappings.get_mut(&ih).unwrap();
            if matches!(
                m.state,
                MappingState::Building { .. } | MappingState::Failed(_)
            ) {
                return;
            }
            let builder = m.nodes[0];
            m.state = MappingState::Building {
                builder,
                known_output: known,
                lease: None,
                phase: BuildPhase::AwaitingInputs,
            };
        }
        let builder = match &self.mappings[&ih].state {
            MappingState::Building { builder, .. } => *builder,
            _ => unreachable!(),
        };
        let ups: Vec<NodeId> = self.dag.nodes[builder.idx()]
            .distinct_upstreams()
            .into_iter()
            .collect();
        for up in ups {
            self.demand_node(up, Reason::BuildInputOf(builder), fx);
        }
        self.try_start_build(builder, fx);
    }

    /// Ready iff every build input is fully realized (present, closure and
    /// all). Idempotent; called whenever an input completes.
    fn try_start_build(&mut self, builder: NodeId, fx: &mut Vec<Effect>) {
        let Some(ih) = self.nodes[builder.idx()].input_hash else {
            return;
        };
        match &self.mappings[&ih].state {
            MappingState::Building {
                builder: b,
                phase: BuildPhase::AwaitingInputs,
                ..
            } if *b == builder => {}
            _ => return,
        }
        let mut protect: BTreeSet<OutputHash> = BTreeSet::new();
        for up in self.dag.nodes[builder.idx()].distinct_upstreams() {
            let Some(uoh) = self.node_output(up) else {
                return;
            };
            if !self.fully_realized(uoh) {
                return;
            }
            protect.insert(uoh);
        }
        // A6: protect the inputs before the build exists
        let store = self.dag.nodes[builder.idx()].target_store.clone();
        let tok = self.tok(Inflight::BuildLease { input: ih });
        let MappingState::Building { phase, .. } = &mut self.mappings.get_mut(&ih).unwrap().state
        else {
            unreachable!()
        };
        *phase = BuildPhase::AwaitingLease;
        fx.push(Effect::AcquireLease {
            tok,
            store,
            protect,
        });
    }

    fn on_lease_granted(&mut self, tok: RequestId, lease: LeaseId, fx: &mut Vec<Effect>) {
        match self.take_inflight(tok) {
            Inflight::BuildLease { input: ih } => {
                let builder = match &self.mappings[&ih].state {
                    MappingState::Building {
                        builder,
                        phase: BuildPhase::AwaitingLease,
                        ..
                    } => *builder,
                    s => panic!("bug: build lease granted in state {s:?}"),
                };
                let (bt, recipe, store, inputs) = {
                    let dn = &self.dag.nodes[builder.idx()];
                    let inputs: Vec<_> = dn
                        .upstreams
                        .iter()
                        .map(|(alias, up)| (alias.clone(), self.node_output(*up).unwrap()))
                        .collect();
                    (
                        dn.builder,
                        dn.recipe.clone(),
                        dn.target_store.clone(),
                        inputs,
                    )
                };
                let tok = self.tok(Inflight::Build { input: ih });
                let MappingState::Building {
                    lease: l, phase, ..
                } = &mut self.mappings.get_mut(&ih).unwrap().state
                else {
                    unreachable!()
                };
                *l = Some(lease);
                *phase = BuildPhase::Running;
                fx.push(Effect::StartBuild {
                    tok,
                    node: builder,
                    input: ih,
                    builder: bt,
                    recipe,
                    store,
                    inputs,
                });
            }
            Inflight::DownloadLease { output: oh } => {
                let (from, to) = {
                    let RealizationState::Downloading {
                        from,
                        to,
                        phase: DownloadPhase::AwaitingLease,
                        ..
                    } = &self.realizations[&oh].state
                    else {
                        panic!("bug: download lease granted in wrong state")
                    };
                    (from.clone(), to.clone())
                };
                let tok = self.tok(Inflight::Download { output: oh });
                let RealizationState::Downloading {
                    lease: l, phase, ..
                } = &mut self.realizations.get_mut(&oh).unwrap().state
                else {
                    unreachable!()
                };
                *l = Some(lease);
                *phase = DownloadPhase::Fetching;
                fx.push(Effect::StartDownload {
                    tok,
                    output: oh,
                    from,
                    to,
                });
            }
            other => panic!("bug: LeaseGranted for {other:?}"),
        }
    }

    fn on_build_finished(&mut self, tok: RequestId, outcome: BuildOutcome, fx: &mut Vec<Effect>) {
        let Inflight::Build { input: ih } = self.take_inflight(tok) else {
            panic!("bug: BuildFinished for a non-build token");
        };
        let (builder, known) = match &self.mappings[&ih].state {
            MappingState::Building {
                builder,
                known_output,
                phase: BuildPhase::Running,
                ..
            } => (*builder, *known_output),
            s => panic!("bug: BuildFinished in state {s:?}"),
        };
        match outcome {
            BuildOutcome::Failure { log } => {
                let fid = self.record_failure(
                    FailureKind::Build,
                    Origin::Node(builder),
                    FailureDetail::BuildLog(log),
                );
                self.fail_mapping(ih, fid, fx);
            }
            BuildOutcome::Success {
                output,
                runtime_refs,
                log: _,
            } => {
                let (bt, store, input_outputs) = {
                    let dn = &self.dag.nodes[builder.idx()];
                    let ios: BTreeSet<OutputHash> = dn
                        .upstreams
                        .iter()
                        .map(|(_, up)| self.node_output(*up).unwrap())
                        .collect();
                    (dn.builder, dn.target_store.clone(), ios)
                };
                // B2/§8 for built nodes: a build can only declare runtime
                // refs to inputs it actually saw
                if !runtime_refs.is_subset(&input_outputs) {
                    let fid = self.record_failure(
                        FailureKind::ClosureEscape,
                        Origin::Node(builder),
                        FailureDetail::Text(
                            "build declared runtime refs outside its inputs".into(),
                        ),
                    );
                    self.fail_mapping(ih, fid, fx);
                    return;
                }
                // A2/B18: a rebuild of a named mapping must reproduce the name
                if let Some(expected) = known
                    && expected != output
                {
                    let kind = match bt {
                        BuilderType::FetchUrl => FailureKind::TofuMismatch,
                        _ => FailureKind::NonDeterminism,
                    };
                    let fid = self.record_failure(
                        kind,
                        Origin::Node(builder),
                        FailureDetail::Text(format!(
                            "expected {expected}, build produced {output}"
                        )),
                    );
                    self.fail_mapping(ih, fid, fx);
                    return;
                }
                let tok = self.tok(Inflight::BuildOutputCommit { input: ih });
                let MappingState::Building { phase, .. } =
                    &mut self.mappings.get_mut(&ih).unwrap().state
                else {
                    unreachable!()
                };
                *phase = BuildPhase::CommittingOutput {
                    output,
                    runtime_refs,
                };
                fx.push(Effect::CommitOutput { tok, store, output });
            }
        }
    }

    fn on_output_committed(&mut self, tok: RequestId, fx: &mut Vec<Effect>) {
        match self.take_inflight(tok) {
            // already_existed (ENOTEMPTY) means a concurrent build won: same
            // bytes either way, proceed identically (B5)
            Inflight::BuildOutputCommit { input: ih } => {
                let builder = match &self.mappings[&ih].state {
                    MappingState::Building {
                        builder,
                        phase: BuildPhase::CommittingOutput { .. },
                        ..
                    } => *builder,
                    s => panic!("bug: OutputCommitted in state {s:?}"),
                };
                let store = self.dag.nodes[builder.idx()].target_store.clone();
                let tok = self.tok(Inflight::BuildMappingCommit { input: ih });
                let MappingState::Building { phase, .. } =
                    &mut self.mappings.get_mut(&ih).unwrap().state
                else {
                    unreachable!()
                };
                let BuildPhase::CommittingOutput {
                    output,
                    runtime_refs,
                } = mem::replace(phase, BuildPhase::AwaitingInputs)
                else {
                    unreachable!()
                };
                *phase = BuildPhase::CommittingMapping {
                    output,
                    runtime_refs,
                };
                fx.push(Effect::CommitMapping {
                    tok,
                    store,
                    input: ih,
                    output,
                });
            }
            Inflight::DownloadCommit { output: oh } => self.finish_download(oh, fx),
            other => panic!("bug: OutputCommitted for {other:?}"),
        }
    }

    fn on_mapping_committed(&mut self, tok: RequestId, result: CommitResult, fx: &mut Vec<Effect>) {
        let Inflight::BuildMappingCommit { input: ih } = self.take_inflight(tok) else {
            panic!("bug: MappingCommitted for a non-commit token");
        };
        match result {
            CommitResult::Conflict { existing } => {
                // feedback blocker 3: EEXIST with a different target — two
                // builds of one input-hash produced different bytes
                let builder = match &self.mappings[&ih].state {
                    MappingState::Building { builder, .. } => *builder,
                    s => panic!("bug: MappingCommitted in state {s:?}"),
                };
                let built = match &self.mappings[&ih].state {
                    MappingState::Building {
                        phase: BuildPhase::CommittingMapping { output, .. },
                        ..
                    } => *output,
                    _ => unreachable!(),
                };
                let fid = self.record_failure(
                    FailureKind::NonDeterminism,
                    Origin::Node(builder),
                    FailureDetail::Text(format!(
                        "store maps input to {existing}, we built {built}"
                    )),
                );
                self.fail_mapping(ih, fid, fx);
            }
            CommitResult::Committed => {
                let m = self.mappings.get_mut(&ih).unwrap();
                let prev = mem::replace(&mut m.state, MappingState::Unresolved);
                let MappingState::Building {
                    builder,
                    known_output,
                    lease,
                    phase:
                        BuildPhase::CommittingMapping {
                            output,
                            runtime_refs,
                        },
                } = prev
                else {
                    panic!("bug: MappingCommitted in state {prev:?}")
                };
                m.state = MappingState::Resolved { output };
                let store = self.dag.nodes[builder.idx()].target_store.clone();
                if let Some(l) = lease {
                    fx.push(Effect::ReleaseLease {
                        store: store.clone(),
                        lease: l,
                    });
                }
                // bytes are present *before* the naming cascade runs, so
                // transferred demand immediately sees a Present realization
                {
                    let r = self.realizations.entry(output).or_default();
                    r.present_in.insert(store.clone());
                    if r.producer.is_none() {
                        r.producer = Some(ih);
                    }
                    if !matches!(r.state, RealizationState::Present { .. }) {
                        r.state = RealizationState::Present { store };
                    }
                }
                self.learn_runtime_refs(output, runtime_refs, fx);
                if known_output.is_none() {
                    // first resolution: record the fact, cascade names
                    let prev = self
                        .knowledge
                        .facts
                        .insert(ih, (output, MappingSource::Built(builder)));
                    assert!(
                        prev.is_none(),
                        "bug: built a mapping that already had a fact"
                    );
                    let nodes = self.mappings[&ih].nodes.clone();
                    for n in nodes {
                        self.on_node_named(n, output, fx);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // downloading

    fn on_download_finished(
        &mut self,
        tok: RequestId,
        outcome: DownloadOutcome,
        fx: &mut Vec<Effect>,
    ) {
        let Inflight::Download { output: oh } = self.take_inflight(tok) else {
            panic!("bug: DownloadFinished for a non-download token");
        };
        match outcome {
            DownloadOutcome::Failure { error } => {
                let fid = self.record_failure(
                    FailureKind::Download,
                    Origin::Output(oh),
                    FailureDetail::Text(error),
                );
                // no fallback to other remotes / local build yet (host retry
                // policy is a later milestone)
                self.fail_realization(oh, fid, fx);
            }
            DownloadOutcome::Success { runtime_refs } => {
                let to = {
                    let RealizationState::Downloading {
                        to,
                        phase: DownloadPhase::Fetching,
                        ..
                    } = &self.realizations[&oh].state
                    else {
                        panic!("bug: DownloadFinished in wrong state")
                    };
                    to.clone()
                };
                let tok = self.tok(Inflight::DownloadCommit { output: oh });
                let RealizationState::Downloading { phase, .. } =
                    &mut self.realizations.get_mut(&oh).unwrap().state
                else {
                    unreachable!()
                };
                *phase = DownloadPhase::Committing { runtime_refs };
                fx.push(Effect::CommitOutput {
                    tok,
                    store: to,
                    output: oh,
                });
            }
        }
    }

    fn finish_download(&mut self, oh: OutputHash, fx: &mut Vec<Effect>) {
        let r = self.realizations.get_mut(&oh).unwrap();
        let prev = mem::replace(&mut r.state, RealizationState::Absent);
        let RealizationState::Downloading {
            to,
            lease,
            phase: DownloadPhase::Committing { runtime_refs },
            ..
        } = prev
        else {
            panic!("bug: finish_download in state {prev:?}")
        };
        r.present_in.insert(to.clone());
        r.state = RealizationState::Present { store: to.clone() };
        if let Some(l) = lease {
            fx.push(Effect::ReleaseLease {
                store: to,
                lease: l,
            });
        }
        // TODO(A12): also write the learned mapping back into the local
        // store (CommitMapping) once the download path grows its second leg
        self.learn_runtime_refs(oh, runtime_refs, fx);
    }

    // ------------------------------------------------------------------
    // failure recording (facts about failure; propagation is derived at
    // report time so it cannot depend on event order)

    fn record_failure(
        &mut self,
        kind: FailureKind,
        origin: Origin,
        detail: FailureDetail,
    ) -> FailureId {
        let fid = FailureId(self.failures.len() as u32);
        self.failures.push(FailureRecord {
            kind,
            origin,
            detail,
        });
        fid
    }

    fn fail_mapping(&mut self, ih: InputHash, fid: FailureId, fx: &mut Vec<Effect>) {
        let m = self.mappings.get_mut(&ih).unwrap();
        let prev = mem::replace(&mut m.state, MappingState::Failed(fid));
        let nodes = m.nodes.clone();
        if let MappingState::Building {
            builder,
            lease: Some(l),
            ..
        } = prev
        {
            let store = self.dag.nodes[builder.idx()].target_store.clone();
            fx.push(Effect::ReleaseLease { store, lease: l });
        }
        for n in nodes {
            let slot = &mut self.nodes[n.idx()];
            if slot.failure.is_none() {
                slot.failure = Some(fid);
            }
        }
    }

    fn fail_realization(&mut self, oh: OutputHash, fid: FailureId, fx: &mut Vec<Effect>) {
        let r = self.realizations.get_mut(&oh).unwrap();
        let prev = mem::replace(&mut r.state, RealizationState::Failed(fid));
        if let RealizationState::Downloading {
            to, lease: Some(l), ..
        } = prev
        {
            fx.push(Effect::ReleaseLease {
                store: to,
                lease: l,
            });
        }
    }

    // ------------------------------------------------------------------
    // quiescence & report

    /// Call once no effects are pending and nothing is inflight. Derives the
    /// per-node end states; upstream blame is assigned here, deterministically
    /// (lowest failed direct upstream), never during the run.
    pub fn quiesced(&mut self) -> Outcome {
        assert!(
            self.inflight.is_empty(),
            "quiesced() called with inflight requests"
        );
        let n = self.dag.nodes.len();
        let mut statuses: Vec<NodeStatus> = Vec::with_capacity(n);
        for i in 0..n {
            let id = NodeId(i as u32);
            let status = if let Some(fid) = self.nodes[i].failure {
                NodeStatus::Failed { failure: fid }
            } else if let Some(up) = self.dag.nodes[i]
                .distinct_upstreams()
                .into_iter()
                .find(|u| matches!(statuses[u.idx()], NodeStatus::Failed { .. }))
            {
                // BTreeSet iteration: the *lowest* failed upstream takes blame
                let fid = self.record_failure(
                    FailureKind::Upstream,
                    Origin::Node(up),
                    FailureDetail::None,
                );
                NodeStatus::Failed { failure: fid }
            } else {
                let oh = self
                    .node_output(id)
                    .expect("bug: quiesced with an unnamed, unfailed node");
                if self.nodes[i].demand.realize.is_empty() {
                    NodeStatus::Named { output: oh }
                } else if self.fully_realized(oh) {
                    let store = self.realizations[&oh].present_in.first().unwrap().clone();
                    NodeStatus::Realized { output: oh, store }
                } else if let Some(fid) = self.realization_blame(oh, &mut BTreeSet::new()) {
                    NodeStatus::Failed { failure: fid }
                } else {
                    panic!(
                        "bug: node {id:?} demanded but neither realized nor failed at quiescence"
                    );
                }
            };
            statuses.push(status);
        }
        let success = self
            .dag
            .targets
            .iter()
            .all(|t| matches!(statuses[t.idx()], NodeStatus::Realized { .. }));
        Outcome {
            statuses,
            failures: self.failures.clone(),
            success,
        }
    }

    fn realization_blame(
        &self,
        oh: OutputHash,
        visited: &mut BTreeSet<OutputHash>,
    ) -> Option<FailureId> {
        if !visited.insert(oh) {
            return None;
        }
        let r = self.realizations.get(&oh)?;
        if let RealizationState::Failed(fid) = r.state {
            return Some(fid);
        }
        for dep in &r.rt_missing {
            if let Some(f) = self.realization_blame(*dep, visited) {
                return Some(f);
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // small helpers

    fn tok(&mut self, kind: Inflight) -> RequestId {
        let t = RequestId(self.next_tok);
        self.next_tok += 1;
        self.inflight.insert(t, kind);
        t
    }

    fn take_inflight(&mut self, tok: RequestId) -> Inflight {
        self.inflight
            .remove(&tok)
            .expect("bug: event for an unknown request token")
    }

    fn mapping_output(&self, ih: InputHash) -> Option<OutputHash> {
        match &self.mappings.get(&ih)?.state {
            MappingState::Resolved { output } => Some(*output),
            MappingState::Building { known_output, .. } => *known_output,
            _ => None,
        }
    }

    /// The output-hash of a node, if its mapping is resolved (or known
    /// during a rebuild).
    pub fn node_output(&self, n: NodeId) -> Option<OutputHash> {
        self.mapping_output(self.nodes[n.idx()].input_hash?)
    }

    /// present + runtime refs known + runtime closure fully realized
    pub fn fully_realized(&self, oh: OutputHash) -> bool {
        self.realizations.get(&oh).is_some_and(|r| {
            matches!(r.state, RealizationState::Present { .. })
                && r.rt_refs.is_some()
                && r.rt_missing.is_empty()
        })
    }

    fn store_rank(&self, s: &StoreName) -> usize {
        self.dag.stores.get_index_of(s).unwrap_or(usize::MAX)
    }

    /// Which local store a download of `oh` lands in: the producing node's
    /// target store when one is known, else the primary (A4).
    fn download_target_store(&self, oh: OutputHash) -> StoreName {
        if let Some(pih) = self.realizations[&oh].producer {
            let n = self.mappings[&pih].nodes[0];
            self.dag.nodes[n.idx()].target_store.clone()
        } else {
            self.dag.primary.clone()
        }
    }
}
