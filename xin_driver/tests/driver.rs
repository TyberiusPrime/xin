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

fn node(recipe: &str, upstreams: &[(&str, &str)], target: bool) -> RawNode {
    RawNode {
        builder: BuilderType::Process,
        recipe: recipe.as_bytes().to_vec(),
        is_target: target,
        target_store: None,
        remotes: ValidRemoteStores::All,
        upstreams: upstreams
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
    let mut d = Driver::new(vec![(
        sn("primary"),
        Backend::Local(LocalStore::open(td.store_root()).unwrap()),
    )]);
    d.sandbox = Some(sandbox());
    d
}

/// Builds are sandbox-mandatory: no bwrap or no bootstrap busybox on this
/// host means the build tests cannot run at all — fail loudly, never fall
/// back to unsandboxed execution.
fn sandbox() -> Sandbox {
    Sandbox::detect(None)
        .expect("build tests need bwrap and a static busybox (dev flake: pkgsStatic.busybox)")
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
    let mut driver = Driver::new(vec![
        (
            sn("primary"),
            Backend::Local(LocalStore::open(td.store_root()).unwrap()),
        ),
        (sn("nowhere"), Backend::DummyRemote),
    ]);
    driver.sandbox = Some(sandbox());
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

// ----------------------------------------------------------- containers

use xin_driver::Sandbox;

#[test]
fn container_build_is_isolated_and_sees_the_b4_layout() {
    let td = TestDir::new("container_isolated");
    // the test dir itself is proof of host visibility: this recipe only
    // succeeds if the host filesystem is NOT there, /nix is NOT there,
    // and the B4 paths ARE
    let recipe = format!(
        r#"
set -e
test ! -e "{host}"
test ! -e /nix
test ! -e /usr
test "$XIN_OUT" = /xin/out
test "$PWD" = /xin/work
echo hello > "$XIN_OUT/payload/greeting"
"#,
        host = td.path.display()
    );
    let top_recipe = r#"
set -e
test -e "/xin/$XIN_INPUT_HASH_base/payload/greeting"
tr a-z A-Z < "$XIN_INPUTS/base/payload/greeting" > "$XIN_OUT/payload/shout"
ln -s "../../$XIN_INPUT_HASH_base" "$XIN_OUT/runtime-inputs/base"
"#;
    let input = raw(vec![
        ("base", node(&recipe, &[], false)),
        ("top", node(top_recipe, &[("base", "base")], true)),
    ]);
    let (r, d, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(out.success, "failures: {:?}", out.failures);
    assert_eq!(d.builds_run, 2);
    let oh = realized_output(&out.statuses[r.dag.id_of("top").unwrap().idx()]);
    let dir = d.local(&sn("primary")).output_dir(oh);
    assert_eq!(
        fs::read_to_string(dir.join("payload/shout")).unwrap(),
        "HELLO\n"
    );
    // the declared ref carries the canonical relocatable form on disk
    let target = fs::read_link(dir.join("runtime-inputs/base")).unwrap();
    assert!(target.to_str().unwrap().starts_with("../../"));
}

#[test]
fn container_build_has_no_network() {
    let td = TestDir::new("container_no_net");
    // no interface but loopback, and even that is down (B3)
    let recipe = r#"
set -e
test "$(ls /sys/class/net 2>/dev/null | grep -v '^lo$' | wc -l)" = 0
echo ok > "$XIN_OUT/payload/f"
"#;
    let input = raw(vec![("probe", node(recipe, &[], true))]);
    let (_, _, out) = resolve(input, driver_for(&td)).unwrap();
    assert!(out.success, "failures: {:?}", out.failures);
}

#[test]
fn build_without_a_sandbox_is_an_error_not_a_fallback() {
    let td = TestDir::new("no_sandbox_no_build");
    let input = raw(vec![("n", node("echo hi", &[], true))]);
    let mut driver = Driver::new(vec![(
        sn("primary"),
        Backend::Local(LocalStore::open(td.store_root()).unwrap()),
    )]);
    driver.sandbox = None;
    let Err(err) = resolve(input, driver).map(|_| ()) else {
        panic!("a build without a sandbox must fail");
    };
    assert!(err.to_string().contains("unsandboxed"), "{err}");
}

// ------------------------------------------------------------- fetchurl

fn fetch_node(url: &str, target: bool) -> RawNode {
    RawNode {
        builder: BuilderType::FetchUrl,
        recipe: url.as_bytes().to_vec(),
        is_target: target,
        target_store: None,
        remotes: ValidRemoteStores::All,
        upstreams: Vec::new(),
        cores: Cores::One,
    }
}

#[test]
fn fetchurl_is_tofu_pinned() {
    let td = TestDir::new("fetch_tofu");
    let src = td.path.join("dataset.csv");
    fs::write(&src, "a,b\n1,2\n").unwrap();
    let url = format!("file://{}", src.display());

    // first fetch: trust on first use — the content gets pinned
    let (r, d, out) =
        resolve(raw(vec![("data", fetch_node(&url, true))]), driver_for(&td)).unwrap();
    assert!(out.success, "failures: {:?}", out.failures);
    assert_eq!(d.builds_run, 1);
    let oh = realized_output(&out.statuses[r.dag.id_of("data").unwrap().idx()]);
    let ih = r.nodes[0].input_hash.unwrap();
    let payload = d.local(&sn("primary")).output_dir(oh).join("payload");
    assert_eq!(
        fs::read_to_string(payload.join("dataset.csv")).unwrap(),
        "a,b\n1,2\n"
    );

    // upstream drift changes nothing: mapping + bytes are pinned, no refetch
    fs::write(&src, "a,b\n9,9\n").unwrap();
    let (_, d2, out2) =
        resolve(raw(vec![("data", fetch_node(&url, true))]), driver_for(&td)).unwrap();
    assert!(out2.success);
    assert_eq!(d2.builds_run, 0, "a pinned fetch must not re-run");
    assert_eq!(
        fs::read_to_string(payload.join("dataset.csv")).unwrap(),
        "a,b\n1,2\n",
        "the pinned content wins over upstream drift"
    );

    // surgery: the bytes vanish but the pinned mapping survives; the
    // refetch reproduces *different* bytes => TofuMismatch (A2), and the
    // recorded mapping is NOT silently moved to the new content
    fs::remove_dir_all(d.local(&sn("primary")).output_dir(oh)).unwrap();
    let (_, _, out3) =
        resolve(raw(vec![("data", fetch_node(&url, true))]), driver_for(&td)).unwrap();
    assert!(!out3.success);
    assert!(
        out3.failures
            .iter()
            .any(|f| f.kind == FailureKind::TofuMismatch),
        "failures: {:?}",
        out3.failures
    );
    assert_eq!(
        d.local(&sn("primary")).lookup_mapping(ih).unwrap(),
        Some(oh),
        "A2: a TOFU mismatch must not update the input->output mapping"
    );
}

#[test]
fn fetchurl_rejects_unknown_schemes_with_a_build_failure() {
    let td = TestDir::new("fetch_bad_scheme");
    let (_, _, out) = resolve(
        raw(vec![("data", fetch_node("gopher://old.example/x", true))]),
        driver_for(&td),
    )
    .unwrap();
    assert!(!out.success);
    assert!(
        out.failures.iter().any(|f| f.kind == FailureKind::Build),
        "failures: {:?}",
        out.failures
    );
}
