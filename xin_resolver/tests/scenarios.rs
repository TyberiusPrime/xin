//! End-to-end scenarios through the sim world, including the first
//! confluence sweep (resolver-architecture.md §9).

use std::collections::BTreeSet;

use xin_resolver::failure::{FailureKind, Origin};
use xin_resolver::hashes::{OutputHash, input_hash_of};
use xin_resolver::input::{
    BuilderType, Cores, HumanName, InputName, RawInput, RawNode, StoreDef, StoreName,
    ValidRemoteStores,
};
use xin_resolver::sim::{BuildScript, Policy, SimWorld, drive, drive_policy, sim_output_for};
use xin_resolver::{NodeStatus, Outcome, Resolver};

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

fn local_only(nodes: Vec<(&str, RawNode)>) -> RawInput {
    RawInput::from_parts(
        nodes.into_iter().map(|(n, r)| (hn(n), r)).collect(),
        vec![(sn("primary"), StoreDef::Local { writeable: true })],
    )
}

fn with_remotes(nodes: Vec<(&str, RawNode)>, remotes: &[&str]) -> RawInput {
    let mut stores = vec![(sn("primary"), StoreDef::Local { writeable: true })];
    for r in remotes {
        stores.push((sn(r), StoreDef::Remote));
    }
    RawInput::from_parts(nodes.into_iter().map(|(n, r)| (hn(n), r)).collect(), stores)
}

fn run(
    raw: RawInput,
    seed: u64,
    setup: impl FnOnce(&Resolver, &mut SimWorld),
) -> (Resolver, SimWorld, Outcome) {
    let dag = raw.ingest().unwrap();
    let mut resolver = Resolver::new(dag);
    let mut world = SimWorld::new(&resolver.dag);
    setup(&resolver, &mut world);
    let out = drive(&mut resolver, &mut world, seed);
    (resolver, world, out)
}

fn status<'a>(r: &Resolver, out: &'a Outcome, name: &str) -> &'a NodeStatus {
    &out.statuses[r.dag.id_of(name).unwrap().idx()]
}

#[test]
fn single_node_builds_and_commits() {
    let (r, w, out) = run(
        local_only(vec![("a", node("hello", &[], true))]),
        42,
        |_, _| {},
    );
    assert!(out.success);
    let ih = input_hash_of(&[], b"hello");
    let oh = sim_output_for(ih);
    assert_eq!(
        *status(&r, &out, "a"),
        NodeStatus::Realized {
            output: oh,
            store: sn("primary")
        }
    );
    let s = w.store(&sn("primary"));
    assert_eq!(s.mappings.get(&ih), Some(&oh));
    assert!(s.outputs.contains_key(&oh));
    assert_eq!(w.builds_run, 1);
    assert!(w.leases.is_empty(), "all leases released at quiescence");
}

#[test]
fn chain_builds_bottom_up_with_runtime_refs() {
    let raw = local_only(vec![
        ("a", node("A", &[], false)),
        ("b", node("rt:a", &[("a", "a")], true)),
    ]);
    let (r, w, out) = run(raw, 7, |_, _| {});
    assert!(out.success);
    let ih_a = input_hash_of(&[], b"A");
    let oh_a = sim_output_for(ih_a);
    let ih_b = input_hash_of(&[("a", oh_a)], b"rt:a");
    let oh_b = sim_output_for(ih_b);
    assert_eq!(
        *status(&r, &out, "a"),
        NodeStatus::Realized {
            output: oh_a,
            store: sn("primary")
        }
    );
    assert_eq!(
        *status(&r, &out, "b"),
        NodeStatus::Realized {
            output: oh_b,
            store: sn("primary")
        }
    );
    assert_eq!(w.builds_run, 2);
    // b's stored runtime refs point at a
    assert_eq!(
        w.store(&sn("primary")).outputs[&oh_b],
        BTreeSet::from([oh_a])
    );
}

#[test]
fn substitution_with_runtime_closure_expansion() {
    // remote knows both mappings and has both outputs; b's runtime refs pull
    // a's *bytes* in without a ever being build-demanded (A7 scope expansion)
    let raw = with_remotes(
        vec![
            ("a", node("A", &[], false)),
            ("b", node("rt:a", &[("a", "a")], true)),
        ],
        &["remote"],
    );
    let ih_a = input_hash_of(&[], b"A");
    let oh_a = OutputHash::of(b"remote-built-a");
    let ih_b = input_hash_of(&[("a", oh_a)], b"rt:a");
    let oh_b = OutputHash::of(b"remote-built-b");
    let (r, w, out) = run(raw, 3, |_, w| {
        let s = w.store_mut(&sn("remote"));
        s.mappings.insert(ih_a, oh_a);
        s.mappings.insert(ih_b, oh_b);
        s.outputs.insert(oh_a, BTreeSet::new());
        s.outputs.insert(oh_b, BTreeSet::from([oh_a]));
    });
    assert!(out.success);
    assert_eq!(w.builds_run, 0, "everything substituted");
    assert_eq!(w.downloads_run, 2);
    assert_eq!(
        *status(&r, &out, "b"),
        NodeStatus::Realized {
            output: oh_b,
            store: sn("primary")
        }
    );
    // a itself was never realize-demanded as a node…
    assert_eq!(*status(&r, &out, "a"), NodeStatus::Named { output: oh_a });
    // …but its bytes are local, pulled in through b's runtime closure
    assert!(w.store(&sn("primary")).outputs.contains_key(&oh_a));
    // A12: mappings learned from the remote were written back locally
    assert_eq!(w.store(&sn("primary")).mappings.get(&ih_a), Some(&oh_a));
    assert_eq!(w.store(&sn("primary")).mappings.get(&ih_b), Some(&oh_b));
}

#[test]
fn early_cutoff_names_without_realizing() {
    // A1: the remote knows a's mapping; b needs only a's *name*, not bytes,
    // because b itself is substituted and has no runtime ref on a
    let raw = with_remotes(
        vec![
            ("a", node("A", &[], false)),
            ("b", node("B", &[("a", "a")], true)),
        ],
        &["remote"],
    );
    let ih_a = input_hash_of(&[], b"A");
    let oh_a = OutputHash::of(b"remote-a");
    let ih_b = input_hash_of(&[("a", oh_a)], b"B");
    let oh_b = OutputHash::of(b"remote-b");
    let (r, w, out) = run(raw, 11, |_, w| {
        let s = w.store_mut(&sn("remote"));
        s.mappings.insert(ih_a, oh_a);
        s.mappings.insert(ih_b, oh_b);
        s.outputs.insert(oh_b, BTreeSet::new()); // a's bytes exist nowhere
    });
    assert!(out.success);
    assert_eq!(w.builds_run, 0);
    assert_eq!(w.downloads_run, 1);
    assert_eq!(*status(&r, &out, "a"), NodeStatus::Named { output: oh_a });
    assert!(!w.store(&sn("primary")).outputs.contains_key(&oh_a));
    // A12: even the bytes-less mapping fact was persisted locally
    assert_eq!(w.store(&sn("primary")).mappings.get(&ih_a), Some(&oh_a));
    assert_eq!(w.store(&sn("primary")).mappings.get(&ih_b), Some(&oh_b));
}

#[test]
fn build_failure_blames_downstream() {
    let raw = local_only(vec![
        ("a", node("A", &[], false)),
        ("b", node("B", &[("a", "a")], true)),
    ]);
    let (r, w, out) = run(raw, 5, |r, w| {
        w.builds.insert(
            r.dag.id_of("a").unwrap(),
            BuildScript::Fail {
                stderr: "boom".into(),
            },
        );
    });
    assert!(!out.success);
    let a = r.dag.id_of("a").unwrap();
    let NodeStatus::Failed { failure: fid_a } = status(&r, &out, "a") else {
        panic!()
    };
    let NodeStatus::Failed { failure: fid_b } = status(&r, &out, "b") else {
        panic!()
    };
    assert_eq!(out.failures[fid_a.idx()].kind, FailureKind::Build);
    assert_eq!(out.failures[fid_b.idx()].kind, FailureKind::Upstream);
    assert_eq!(out.failures[fid_b.idx()].origin, Origin::Node(a));
    // §7: the chain is reconstructed by walking origins
    assert_eq!(out.chain(*fid_b), vec![*fid_b, *fid_a]);
    assert!(w.leases.is_empty());
}

#[test]
fn conflicting_store_answers_fail_the_node() {
    // A8: two remotes disagree on the mapping → build failure for the node
    let raw = with_remotes(vec![("a", node("A", &[], true))], &["r1", "r2"]);
    let ih_a = input_hash_of(&[], b"A");
    let (r, _w, out) = run(raw, 9, |_, w| {
        w.store_mut(&sn("r1"))
            .mappings
            .insert(ih_a, OutputHash::of(b"x"));
        w.store_mut(&sn("r2"))
            .mappings
            .insert(ih_a, OutputHash::of(b"y"));
    });
    assert!(!out.success);
    let NodeStatus::Failed { failure } = status(&r, &out, "a") else {
        panic!()
    };
    assert_eq!(
        out.failures[failure.idx()].kind,
        FailureKind::MappingConflict
    );
}

#[test]
fn download_failure_blames_the_dependent_target() {
    let raw = with_remotes(vec![("a", node("A", &[], true))], &["remote"]);
    let ih_a = input_hash_of(&[], b"A");
    let oh_a = OutputHash::of(b"remote-a");
    let (r, _w, out) = run(raw, 13, |_, w| {
        let s = w.store_mut(&sn("remote"));
        s.mappings.insert(ih_a, oh_a);
        s.outputs.insert(oh_a, BTreeSet::new());
        w.fail_downloads.insert(oh_a);
    });
    assert!(!out.success);
    let NodeStatus::Failed { failure } = status(&r, &out, "a") else {
        panic!()
    };
    assert_eq!(out.failures[failure.idx()].kind, FailureKind::Download);
    assert_eq!(out.failures[failure.idx()].origin, Origin::Output(oh_a));
}

#[test]
fn shared_input_hash_builds_once() {
    // A7 DAG collapse: two human-named nodes, byte-identical recipe and
    // inputs → one mapping, one build, both realized
    let raw = local_only(vec![
        ("one", node("same", &[], true)),
        ("two", node("same", &[], true)),
    ]);
    let (r, w, out) = run(raw, 17, |_, _| {});
    assert!(out.success);
    assert_eq!(w.builds_run, 1);
    let oh = sim_output_for(input_hash_of(&[], b"same"));
    for n in ["one", "two"] {
        assert_eq!(
            *status(&r, &out, n),
            NodeStatus::Realized {
                output: oh,
                store: sn("primary")
            }
        );
    }
}

/// The headline DST property (M5 preview): for a fixed DAG and fixed world
/// behavior, final store state, statuses, and failure kinds are identical
/// under every interleaving. Mixed scenario: one substituted node, two
/// builds, a runtime-closure edge, plus a failing extra target (keep-going).
#[test]
fn confluence_across_seeds() {
    let ih_base = input_hash_of(&[], b"B");
    let oh_base = sim_output_for(ih_base);
    let ih_left = input_hash_of(&[("base", oh_base)], b"rt:base");
    let oh_left = OutputHash::of(b"remote-left");

    let scenario = || {
        with_remotes(
            vec![
                ("base", node("B", &[], false)),
                ("left", node("rt:base", &[("base", "base")], false)),
                ("right", node("R", &[("base", "base")], false)),
                (
                    "top",
                    node("rt:left", &[("left", "left"), ("right", "right")], true),
                ),
                ("doomed", node("D", &[], true)),
            ],
            &["remote"],
        )
    };

    let mut fingerprints: Vec<String> = Vec::new();
    for seed in 0..32 {
        let (_r, w, out) = run(scenario(), seed, |r, w| {
            let s = w.store_mut(&sn("remote"));
            s.mappings.insert(ih_left, oh_left);
            s.outputs.insert(oh_left, BTreeSet::from([oh_base]));
            w.builds.insert(
                r.dag.id_of("doomed").unwrap(),
                BuildScript::Fail {
                    stderr: "no".into(),
                },
            );
        });
        assert!(!out.success); // doomed fails; everything else realizes
        let kinds: Vec<FailureKind> = out.failures.iter().map(|f| f.kind).collect();
        fingerprints.push(format!("{:?}\n{:?}\n{:?}", out.statuses, kinds, w.stores));
        assert_eq!(w.builds_run, 4); // base, right, top, doomed(failed)
        assert_eq!(w.downloads_run, 1); // left
        assert!(w.leases.is_empty());
    }
    for fp in &fingerprints[1..] {
        assert_eq!(
            *fp, fingerprints[0],
            "end conditions must not depend on the schedule"
        );
    }
}

#[test]
fn substituted_closure_escape_is_detected() {
    // §8: b's remote mapping claims a runtime ref that is not inside b's
    // build closure ({a's output}) — the offending store's claim fails the
    // node instead of silently pulling foreign bytes
    let raw = with_remotes(
        vec![
            ("a", node("A", &[], false)),
            ("b", node("B", &[("a", "a")], true)),
        ],
        &["remote"],
    );
    let ih_a = input_hash_of(&[], b"A");
    let oh_a = sim_output_for(ih_a); // a is built locally: no store knows it
    let ih_b = input_hash_of(&[("a", oh_a)], b"B");
    let oh_b = OutputHash::of(b"remote-b");
    let oh_evil = OutputHash::of(b"not-in-any-closure");
    let (r, _w, out) = run(raw, 21, |_, w| {
        let s = w.store_mut(&sn("remote"));
        s.mappings.insert(ih_b, oh_b);
        s.outputs.insert(oh_b, BTreeSet::from([oh_evil]));
    });
    assert!(!out.success);
    let NodeStatus::Failed { failure } = status(&r, &out, "b") else {
        panic!()
    };
    let rec = &out.failures[failure.idx()];
    assert_eq!(rec.kind, FailureKind::ClosureEscape);
    assert_eq!(rec.origin, Origin::Node(r.dag.id_of("b").unwrap()));
}

#[test]
fn contradicting_refs_claim_fails_the_output() {
    // the remote claims refs {base} in its presence answer but the fetched
    // tree declares {} — a CAS object has exactly one ref set, so a
    // disagreeing source is lying or corrupt
    let raw = with_remotes(
        vec![
            ("base", node("B", &[], false)),
            ("a", node("A", &[("base", "base")], true)),
        ],
        &["remote"],
    );
    let ih_base = input_hash_of(&[], b"B");
    let oh_base = sim_output_for(ih_base);
    let ih_a = input_hash_of(&[("base", oh_base)], b"A");
    let oh_a = OutputHash::of(b"remote-a");
    let (r, _w, out) = run(raw, 23, |_, w| {
        let s = w.store_mut(&sn("remote"));
        s.mappings.insert(ih_a, oh_a);
        s.outputs.insert(oh_a, BTreeSet::new());
        s.claim_overrides.insert(oh_a, BTreeSet::from([oh_base]));
    });
    assert!(!out.success);
    let NodeStatus::Failed { failure } = status(&r, &out, "a") else {
        panic!()
    };
    let rec = &out.failures[failure.idx()];
    assert_eq!(rec.kind, FailureKind::NonDeterminism);
    assert_eq!(rec.origin, Origin::Output(oh_a));
}

/// §7's fail-fast property: under every schedule, the fact set is a subset
/// of the keep-going run's and no fact contradicts it; every lease is
/// released; the originating failure is still reported.
#[test]
fn fail_fast_facts_are_a_consistent_subset() {
    let scenario = || {
        local_only(vec![
            ("doomed", node("X", &[], true)),
            ("c1", node("C1", &[], false)),
            ("c2", node("C2", &[("c1", "c1")], false)),
            ("c3", node("C3", &[("c2", "c2")], true)),
        ])
    };
    let fail_doomed = |r: &Resolver, w: &mut SimWorld| {
        w.builds.insert(
            r.dag.id_of("doomed").unwrap(),
            BuildScript::Fail {
                stderr: "no".into(),
            },
        );
    };
    let (kg_r, kg_w, kg_out) = run(scenario(), 1, fail_doomed);
    assert!(!kg_out.success);
    assert!(matches!(
        *status(&kg_r, &kg_out, "c3"),
        NodeStatus::Realized { .. }
    ));

    for seed in 0..16 {
        let dag = scenario().ingest().unwrap();
        let mut r = Resolver::new(dag);
        let mut w = SimWorld::new(&r.dag);
        fail_doomed(&r, &mut w);
        let out = drive_policy(&mut r, &mut w, seed, Policy::FailFast);
        assert!(!out.success);
        for (ih, (oh, _)) in &r.knowledge.facts {
            assert_eq!(
                kg_r.knowledge.facts.get(ih).map(|(o, _)| o),
                Some(oh),
                "fail-fast learned a fact the keep-going run does not agree with"
            );
        }
        for (ih, oh) in &w.store(&sn("primary")).mappings {
            assert_eq!(kg_w.store(&sn("primary")).mappings.get(ih), Some(oh));
        }
        assert!(w.builds_run <= kg_w.builds_run);
        assert!(w.leases.is_empty(), "cancellation must release every lease");
        let NodeStatus::Failed { failure } = status(&r, &out, "doomed") else {
            panic!("the originating failure must always be reported")
        };
        assert_eq!(out.failures[failure.idx()].kind, FailureKind::Build);
    }
}
