//! Deterministic simulation testing (M5/M6 start, resolver-architecture.md
//! §9): exhaustive interleaving enumeration for small scenarios, and
//! proptest-generated DAGs / failure schedules / shrinkable schedules for
//! the rest. The headline property: for a fixed DAG and fixed world
//! behavior, final store state and learned facts are identical under every
//! interleaving (keep-going); fail-fast runs produce a consistent subset.

use std::collections::BTreeSet;

use proptest::prelude::*;
use xin_resolver::hashes::InputHash;
use xin_resolver::input::{
    BuilderType, Cores, HumanName, InputName, RawInput, RawNode, StoreDef, StoreName,
    ValidRemoteStores,
};
use xin_resolver::sim::{
    BuildScript, Policy, SimWorld, drive_policy, drive_with_choices, explore_all_interleavings,
    fingerprint,
};
use xin_resolver::{Outcome, Resolver};

fn hn(s: &str) -> HumanName {
    HumanName::new(s).unwrap()
}

fn sn(s: &str) -> StoreName {
    StoreName::new(s).unwrap()
}

fn node(recipe: &str, ups: &[(&str, &str)], target: bool) -> RawNode {
    RawNode {
        builder: BuilderType::Process,
        recipe: recipe.as_bytes().to_vec(),
        is_target: target,
        target_store: None,
        remotes: ValidRemoteStores::All,
        upstreams: ups
            .iter()
            .map(|(a, u)| (InputName::new(a).unwrap(), hn(u)))
            .collect(),
        cores: Cores::One,
    }
}

fn make(raw: RawInput, setup: impl FnOnce(&Resolver, &mut SimWorld)) -> (Resolver, SimWorld) {
    let resolver = Resolver::new(raw.ingest().unwrap());
    let mut world = SimWorld::new(&resolver.dag);
    setup(&resolver, &mut world);
    (resolver, world)
}

fn run_seed(
    raw: RawInput,
    seed: u64,
    policy: Policy,
    setup: impl FnOnce(&Resolver, &mut SimWorld),
) -> (Resolver, SimWorld, Outcome) {
    let (mut r, mut w) = make(raw, setup);
    let out = drive_policy(&mut r, &mut w, seed, policy);
    (r, w, out)
}

// ---------------------------------------------------------------------
// exhaustive model checking (every interleaving, small scenarios)

#[test]
fn exhaustive_two_independent_builds() {
    let raw = || {
        RawInput::from_parts(
            vec![
                (hn("a"), node("A", &[], true)),
                (hn("b"), node("B", &[], true)),
            ],
            vec![(sn("primary"), StoreDef::Local { writeable: true })],
        )
    };
    let (fps, runs) =
        explore_all_interleavings(|| make(raw(), |_, _| {}), Policy::KeepGoing, 200_000);
    assert_eq!(
        fps.len(),
        1,
        "confluence violated across {runs} interleavings"
    );
    assert!(runs > 100, "expected real branching, got {runs} runs");
}

#[test]
fn exhaustive_substituted_node_with_write_back() {
    // one node, known to the remote: exercises every ordering of the query
    // wave (2 stores), presence wave (2 stores), A12 write-back commit, and
    // the download pipeline
    let raw = || {
        RawInput::from_parts(
            vec![(hn("a"), node("A", &[], true))],
            vec![
                (sn("primary"), StoreDef::Local { writeable: true }),
                (sn("remote"), StoreDef::Remote),
            ],
        )
    };
    let ih = xin_resolver::hashes::input_hash_of(&[], b"A");
    let oh = xin_resolver::hashes::OutputHash::of(b"remote-a");
    let (fps, runs) = explore_all_interleavings(
        || {
            make(raw(), |_, w| {
                let s = w.store_mut(&sn("remote"));
                s.mappings.insert(ih, oh);
                s.outputs.insert(oh, BTreeSet::new());
            })
        },
        Policy::KeepGoing,
        200_000,
    );
    assert_eq!(
        fps.len(),
        1,
        "confluence violated across {runs} interleavings"
    );
    assert!(runs > 10);
}

#[test]
fn exhaustive_failure_with_independent_survivor() {
    // keep-going: a fails, c (downstream) is poisoned, b succeeds — the end
    // state must not depend on how the failure interleaves with b's build
    let raw = || {
        RawInput::from_parts(
            vec![
                (hn("a"), node("A", &[], false)),
                (hn("b"), node("B", &[], true)),
                (hn("c"), node("C", &[("a", "a")], true)),
            ],
            vec![(sn("primary"), StoreDef::Local { writeable: true })],
        )
    };
    let (fps, runs) = explore_all_interleavings(
        || {
            make(raw(), |r, w| {
                w.builds.insert(
                    r.dag.id_of("a").unwrap(),
                    BuildScript::Fail { stderr: "x".into() },
                );
            })
        },
        Policy::KeepGoing,
        200_000,
    );
    assert_eq!(
        fps.len(),
        1,
        "confluence violated across {runs} interleavings"
    );
    assert!(runs > 50);
}

// ---------------------------------------------------------------------
// proptest: random DAGs, failure schedules, shrinkable schedules

/// (upstream edges as (edge, is-runtime-ref) per earlier node, target flag,
/// recipe salt) per node. Interpretation clamps rows to valid prefixes.
type DagSpec = Vec<(Vec<(bool, bool)>, bool, u8)>;

fn arb_dag() -> impl Strategy<Value = DagSpec> {
    prop::collection::vec(
        (
            prop::collection::vec((any::<bool>(), any::<bool>()), 0..6),
            any::<bool>(),
            0u8..3,
        ),
        1..=6,
    )
}

fn spec_to_raw(spec: &DagSpec, stores: Vec<(StoreName, StoreDef)>) -> RawInput {
    let n = spec.len();
    let mut nodes = Vec::with_capacity(n);
    for (i, (pairs, target, salt)) in spec.iter().enumerate() {
        let mut ups = Vec::new();
        let mut rt_aliases = Vec::new();
        for (j, (edge, rt)) in pairs.iter().take(i).enumerate() {
            if *edge {
                let alias = format!("u{j}");
                ups.push((alias.clone(), format!("n{j}")));
                if *rt {
                    rt_aliases.push(alias);
                }
            }
        }
        let recipe = if rt_aliases.is_empty() {
            format!("p{salt}") // identical salts let zero-input recipes collapse (A7)
        } else {
            format!("rt:{}", rt_aliases.join(","))
        };
        let raw = RawNode {
            builder: BuilderType::Process,
            recipe: recipe.into_bytes(),
            is_target: *target || i == n - 1, // at least one target
            target_store: None,
            remotes: ValidRemoteStores::All,
            upstreams: ups
                .iter()
                .map(|(a, u)| (InputName::new(a).unwrap(), HumanName::new(u).unwrap()))
                .collect(),
            cores: Cores::One,
        };
        nodes.push((hn(&format!("n{i}")), raw));
    }
    RawInput::from_parts(nodes, stores)
}

fn local_stores() -> Vec<(StoreName, StoreDef)> {
    vec![(sn("primary"), StoreDef::Local { writeable: true })]
}

fn local_and_remote() -> Vec<(StoreName, StoreDef)> {
    vec![
        (sn("primary"), StoreDef::Local { writeable: true }),
        (sn("remote"), StoreDef::Remote),
    ]
}

fn include_in_remote(ih: &InputHash, mask: u64) -> bool {
    (mask >> (ih.0[0] % 64)) & 1 == 1
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Random DAG, remote pre-seeded with a random subset of the true facts
    /// (learned from a no-store reference run): every schedule reaches the
    /// same store state and the same facts as building everything from
    /// scratch. Statuses are a *weakening* of the reference: same output
    /// hash everywhere, but early cutoff (A1) may leave a node Named where
    /// the build-everything run realized it.
    #[test]
    fn substitution_is_confluent_and_equivalent(spec in arb_dag(), remote_mask: u64) {
        let (kg_r, kg_w, kg_out) =
            run_seed(spec_to_raw(&spec, local_stores()), 0, Policy::KeepGoing, |_, _| {});
        prop_assert!(kg_out.success);
        // with nothing substitutable, every node in the pruned DAG gets built
        // and therefore realized
        for st in &kg_out.statuses {
            let realized = matches!(st, xin_resolver::NodeStatus::Realized { .. });
            prop_assert!(realized, "reference run left a node unrealized: {:?}", st);
        }

        let seed_remote = |w: &mut SimWorld| {
            for (ih, (oh, _)) in &kg_r.knowledge.facts {
                if include_in_remote(ih, remote_mask) {
                    let refs = kg_w.store(&sn("primary")).outputs[oh].clone();
                    let s = w.store_mut(&sn("remote"));
                    s.mappings.insert(*ih, *oh);
                    s.outputs.insert(*oh, refs);
                }
            }
        };

        let mut fps: Vec<String> = Vec::new();
        for seed in 0..3u64 {
            let (r, w, out) = run_seed(
                spec_to_raw(&spec, local_and_remote()),
                seed,
                Policy::KeepGoing,
                |_, w| seed_remote(w),
            );
            prop_assert!(out.success);
            for (i, st) in out.statuses.iter().enumerate() {
                let xin_resolver::NodeStatus::Realized { output: ref_oh, store: ref_store } =
                    &kg_out.statuses[i]
                else { unreachable!() };
                match st {
                    xin_resolver::NodeStatus::Realized { output, store } => {
                        prop_assert_eq!(output, ref_oh);
                        prop_assert_eq!(store, ref_store);
                    }
                    xin_resolver::NodeStatus::Named { output } => prop_assert_eq!(output, ref_oh),
                    other => prop_assert!(false, "unexpected status {:?}", other),
                }
            }
            for (ih, (oh, _)) in &kg_r.knowledge.facts {
                prop_assert_eq!(r.knowledge.facts.get(ih).map(|(o, _)| o), Some(oh));
            }
            prop_assert_eq!(r.knowledge.facts.len(), kg_r.knowledge.facts.len());
            fps.push(fingerprint(&out, &w));
        }
        for fp in &fps[1..] {
            prop_assert_eq!(fp, &fps[0]);
        }
    }

    /// Random DAG + random failing builders: keep-going is confluent, and
    /// fail-fast under any schedule yields a consistent subset of the
    /// keep-going facts with every lease released.
    #[test]
    fn failure_schedules_keep_going_confluent_fail_fast_subset(
        spec in arb_dag(),
        fail_bits: u8,
    ) {
        let script = |r: &Resolver, w: &mut SimWorld| {
            for i in 0..r.dag.nodes.len() {
                if (fail_bits >> (i % 8)) & 1 == 1 {
                    w.builds.insert(
                        xin_resolver::input::NodeId(i as u32),
                        BuildScript::Fail { stderr: "injected".into() },
                    );
                }
            }
        };
        let (kg_r, kg_w, _kg_out) =
            run_seed(spec_to_raw(&spec, local_stores()), 0, Policy::KeepGoing, script);
        let kg_fp = {
            let (mut r, mut w) = make(spec_to_raw(&spec, local_stores()), script);
            let out = drive_policy(&mut r, &mut w, 0, Policy::KeepGoing);
            fingerprint(&out, &w)
        };
        for seed in 1..4u64 {
            let (_r, w, out) =
                run_seed(spec_to_raw(&spec, local_stores()), seed, Policy::KeepGoing, script);
            prop_assert_eq!(fingerprint(&out, &w), kg_fp.clone());
            prop_assert!(w.leases.is_empty());
        }
        for seed in 0..2u64 {
            let (r, w, _out) =
                run_seed(spec_to_raw(&spec, local_stores()), seed, Policy::FailFast, script);
            for (ih, (oh, _)) in &r.knowledge.facts {
                prop_assert_eq!(kg_r.knowledge.facts.get(ih).map(|(o, _)| o), Some(oh));
            }
            for (ih, oh) in &w.store(&sn("primary")).mappings {
                prop_assert_eq!(kg_w.store(&sn("primary")).mappings.get(ih), Some(oh));
            }
            prop_assert!(w.leases.is_empty());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Schedule as data: an arbitrary (and shrinkable) choice list must land
    /// on the same end state as the canonical all-zeros schedule, on a mixed
    /// scenario with substitution, a failure, and closure expansion.
    #[test]
    fn any_schedule_matches_the_canonical_run(choices in prop::collection::vec(any::<u8>(), 0..300)) {
        let scenario = || {
            RawInput::from_parts(
                vec![
                    (hn("base"), node("B", &[], false)),
                    (hn("left"), node("rt:base", &[("base", "base")], false)),
                    (hn("right"), node("R", &[("base", "base")], false)),
                    (hn("top"), node("rt:left", &[("left", "left"), ("right", "right")], true)),
                    (hn("doomed"), node("D", &[], true)),
                ],
                local_and_remote(),
            )
        };
        let ih_base = xin_resolver::hashes::input_hash_of(&[], b"B");
        let oh_base = xin_resolver::sim::sim_output_for(ih_base);
        let ih_left = xin_resolver::hashes::input_hash_of(&[("base", oh_base)], b"rt:base");
        let oh_left = xin_resolver::hashes::OutputHash::of(b"remote-left");
        let setup = |r: &Resolver, w: &mut SimWorld| {
            let s = w.store_mut(&sn("remote"));
            s.mappings.insert(ih_left, oh_left);
            s.outputs.insert(oh_left, BTreeSet::from([oh_base]));
            w.builds.insert(r.dag.id_of("doomed").unwrap(), BuildScript::Fail { stderr: "no".into() });
        };
        let reference = {
            let (mut r, mut w) = make(scenario(), setup);
            let out = drive_with_choices(&mut r, &mut w, &[], Policy::KeepGoing);
            fingerprint(&out, &w)
        };
        let (mut r, mut w) = make(scenario(), setup);
        let out = drive_with_choices(&mut r, &mut w, &choices, Policy::KeepGoing);
        prop_assert_eq!(fingerprint(&out, &w), reference);
        prop_assert!(w.leases.is_empty());
    }
}
