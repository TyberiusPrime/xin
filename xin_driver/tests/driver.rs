//! Integration tests over real filesystem stores and subprocess builds.
//! Each test gets its own directory under the cargo target tmpdir; on
//! failure it is kept (and its path printed) for inspection, otherwise
//! removed. A stale kept directory from a previous failing run is cleared
//! on the next start.

use std::fs;
use std::path::{Path, PathBuf};

use xin_driver::{Backend, Driver, LocalStore, resolve};
use xin_resolver::NodeStatus;
use xin_resolver::failure::FailureKind;
use xin_resolver::hashes::OutputHash;
use xin_resolver::input::{
    BuilderType, Cores, HumanName, InputName, RawInput, RawNode, StoreDef, StoreName,
    ValidRemoteStores,
};

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> TestDir {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        if path.exists() {
            fs::remove_dir_all(&path).expect("clearing stale test dir from a previous failed run");
        }
        fs::create_dir_all(&path).unwrap();
        TestDir { path }
    }

    fn store_root(&self) -> PathBuf {
        self.path.join("store")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("test dir kept for inspection: {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

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

fn raw(nodes: Vec<(&str, RawNode)>) -> RawInput {
    RawInput::from_parts(
        nodes.into_iter().map(|(n, r)| (hn(n), r)).collect(),
        vec![(sn("primary"), StoreDef::Local { writeable: true })],
    )
}

fn driver_for(td: &TestDir) -> Driver {
    Driver::new(vec![(
        sn("primary"),
        Backend::Local(LocalStore::open(td.store_root()).unwrap()),
    )])
}

fn realized_output(status: &NodeStatus) -> OutputHash {
    match status {
        NodeStatus::Realized { output, .. } => *output,
        other => panic!("expected Realized, got {other:?}"),
    }
}

#[test]
fn single_build_lands_in_the_store_layout() {
    let td = TestDir::new("single_build");
    let input = raw(vec![(
        "hello",
        node(
            r#"echo -n "hello world" > "$XIN_OUT/payload/greeting""#,
            &[],
            true,
        ),
    )]);
    let (r, d, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(out.success, "statuses: {:?}", out.statuses);
    assert_eq!(d.builds_run, 1);

    let oh = realized_output(&out.statuses[r.dag.id_of("hello").unwrap().idx()]);
    let store = d.local(&sn("primary"));
    let node_dir = store.output_dir(oh);
    assert_eq!(
        fs::read_to_string(node_dir.join("payload/greeting")).unwrap(),
        "hello world"
    );
    assert!(node_dir.join("runtime-inputs").is_dir());
    // inputs/<ih> symlink resolves the mapping back
    let ih = r.nodes[0].input_hash.unwrap();
    assert_eq!(store.lookup_mapping(ih).unwrap(), Some(oh));
    // A6 bookkeeping fully unwound, temp drained
    assert_eq!(store.held_leases().unwrap(), 0);
    assert_eq!(
        fs::read_dir(td.store_root().join("temp")).unwrap().count(),
        0
    );
    // build meta captured
    assert!(store.root.join("meta/by_input").is_dir());
}

#[test]
fn chain_flows_bytes_and_declares_runtime_refs() {
    let td = TestDir::new("chain_runtime_refs");
    let input = raw(vec![
        (
            "base",
            node(
                r#"echo -n "payload-of-base" > "$XIN_OUT/payload/data""#,
                &[],
                false,
            ),
        ),
        (
            "top",
            node(
                r#"
                cat "$XIN_INPUTS/base/payload/data" > "$XIN_OUT/payload/copied"
                echo -n " and more" >> "$XIN_OUT/payload/copied"
                ln -s "../../$XIN_INPUT_HASH_base" "$XIN_OUT/runtime-inputs/base"
                "#,
                &[("base", "base")],
                true,
            ),
        ),
    ]);
    let (r, d, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(
        out.success,
        "statuses: {:?}\n{}",
        out.statuses,
        r.trace_report()
    );
    assert_eq!(d.builds_run, 2);

    let oh_base = realized_output(&out.statuses[r.dag.id_of("base").unwrap().idx()]);
    let oh_top = realized_output(&out.statuses[r.dag.id_of("top").unwrap().idx()]);
    let store = d.local(&sn("primary"));
    assert_eq!(
        fs::read_to_string(store.output_dir(oh_top).join("payload/copied")).unwrap(),
        "payload-of-base and more"
    );
    // the declared runtime ref is on disk in canonical A1 form
    let link = fs::read_link(store.output_dir(oh_top).join("runtime-inputs/base")).unwrap();
    assert_eq!(link.to_str().unwrap(), format!("../../{oh_base}"));
}

#[test]
fn second_run_resumes_from_the_store_without_building() {
    let td = TestDir::new("resume");
    let mk = || {
        raw(vec![
            (
                "base",
                node(r#"echo -n "b" > "$XIN_OUT/payload/f""#, &[], false),
            ),
            (
                "top",
                node(
                    r#"
                    cat "$XIN_INPUTS/base/payload/f" > "$XIN_OUT/payload/f"
                    ln -s "../../$XIN_INPUT_HASH_base" "$XIN_OUT/runtime-inputs/base"
                    "#,
                    &[("base", "base")],
                    true,
                ),
            ),
        ])
    };
    let (_, d1, out1) = resolve(mk(), driver_for(&td)).unwrap();
    assert!(out1.success);
    assert_eq!(d1.builds_run, 2);

    // A12: the store *is* the state — a fresh resolver rediscovers the
    // mappings and presence and has nothing left to do
    let (_, d2, out2) = resolve(mk(), driver_for(&td)).unwrap();
    assert!(out2.success);
    assert_eq!(d2.builds_run, 0, "resume must not rebuild anything");
}

#[test]
fn failing_build_reports_log_and_keeps_the_build_dir() {
    let td = TestDir::new("failing_build");
    let input = raw(vec![
        (
            "bad",
            node(r#"echo "something went wrong" >&2; exit 3"#, &[], false),
        ),
        ("wants-bad", node("true", &[("bad", "bad")], true)),
    ]);
    let (r, d, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(!out.success);

    let NodeStatus::Failed { failure } = &out.statuses[r.dag.id_of("bad").unwrap().idx()] else {
        panic!("bad must fail");
    };
    let rec = &out.failures[failure.idx()];
    assert_eq!(rec.kind, FailureKind::Build);
    let xin_resolver::failure::FailureDetail::BuildLog(log) = &rec.detail else {
        panic!("expected a build log");
    };
    assert_eq!(log.return_code, 3);
    assert_eq!(
        String::from_utf8_lossy(&log.stderr),
        "something went wrong\n"
    );
    // downstream poisoned, nothing of bad committed
    assert!(matches!(
        out.statuses[r.dag.id_of("wants-bad").unwrap().idx()],
        NodeStatus::Failed { .. }
    ));
    // the failed build dir sticks around in temp/ for inspection (B5)…
    assert_eq!(
        fs::read_dir(td.store_root().join("temp")).unwrap().count(),
        1
    );
    // …the meta log too, and every lease is released
    let store = d.local(&sn("primary"));
    assert_eq!(store.held_leases().unwrap(), 0);
    let ih = r.nodes[r.dag.id_of("bad").unwrap().idx()]
        .input_hash
        .unwrap();
    let ih_str = ih.to_string();
    let meta = store
        .root
        .join("meta/by_input")
        .join(&ih_str[..2])
        .join(&ih_str);
    assert_eq!(fs::read_to_string(meta.join("exit")).unwrap(), "3\n");
}

#[test]
fn dummy_remote_is_asked_and_always_misses() {
    let td = TestDir::new("dummy_remote");
    let input = RawInput::from_parts(
        vec![(
            hn("a"),
            node(r#"echo -n x > "$XIN_OUT/payload/f""#, &[], true),
        )],
        vec![
            (sn("primary"), StoreDef::Local { writeable: true }),
            (sn("nowhere"), StoreDef::Remote),
        ],
    );
    let driver = Driver::new(vec![
        (
            sn("primary"),
            Backend::Local(LocalStore::open(td.store_root()).unwrap()),
        ),
        (sn("nowhere"), Backend::DummyRemote),
    ]);
    let (_, d, out) = resolve(input, driver).unwrap();
    assert!(out.success);
    assert_eq!(d.builds_run, 1);
}

#[test]
fn builds_are_reproducible_across_stores() {
    // the same recipe in two fresh stores must produce the same output hash
    // (tree hashing ignores times; names/contents/exec/symlinks only)
    let recipe = r#"
        echo -n "data" > "$XIN_OUT/payload/f"
        chmod +x "$XIN_OUT/payload/f"
        mkdir "$XIN_OUT/payload/sub"
        ln -s "f" "$XIN_OUT/payload/sub/link"
    "#;
    let run = |name: &str| {
        let td = TestDir::new(name);
        let (r, _, out) =
            resolve(raw(vec![("n", node(recipe, &[], true))]), driver_for(&td)).unwrap();
        assert!(out.success);
        realized_output(&out.statuses[r.dag.id_of("n").unwrap().idx()])
    };
    assert_eq!(run("repro_one"), run("repro_two"));
}

#[test]
fn malformed_runtime_ref_declaration_fails_the_build() {
    let td = TestDir::new("malformed_ref");
    // a runtime-inputs entry that is a file, not a canonical symlink (B1 lint)
    let input = raw(vec![(
        "bad-decl",
        node(r#"echo -n x > "$XIN_OUT/runtime-inputs/oops""#, &[], true),
    )]);
    let (r, _, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(!out.success);
    let NodeStatus::Failed { failure } = &out.statuses[r.dag.id_of("bad-decl").unwrap().idx()]
    else {
        panic!("must fail");
    };
    assert_eq!(out.failures[failure.idx()].kind, FailureKind::Build);
}
