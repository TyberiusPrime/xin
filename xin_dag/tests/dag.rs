//! The definition pipeline end-to-end: TOML parsing/validation units, and
//! Nickel evaluation (embedded interpreter) of a generated DAG that is then
//! ingested and driven through the resolver's sim world.

use std::fs;
use std::path::Path;

use xin_dag::{DagError, load, parse_toml};
use xin_resolver::Resolver;
use xin_resolver::input::{BuilderType, Cores, StoreDef, StoreName, ValidRemoteStores};
use xin_resolver::sim::{SimWorld, drive};

fn sn(s: &str) -> StoreName {
    StoreName::new(s).unwrap()
}

#[test]
fn toml_defaults_fill_in() {
    let bundle = parse_toml(
        r#"
        [stores.primary]
        type = "local"
        path = "./store"

        [nodes.hello]
        recipe = "echo hi"
        target = true
        "#,
    )
    .unwrap();
    let node = &bundle.raw.nodes[0];
    assert_eq!(node.builder, BuilderType::Process);
    assert_eq!(node.cores, Cores::One);
    assert_eq!(node.remotes, ValidRemoteStores::All);
    assert!(node.is_target);
    assert_eq!(
        bundle.raw.stores[&sn("primary")],
        StoreDef::Local { writeable: true }
    );
    let dag = bundle.raw.ingest().unwrap();
    assert_eq!(dag.primary, sn("primary"));
}

#[test]
fn explicit_primary_orders_the_stores() {
    let bundle = parse_toml(
        r#"
        primary = "zshared"

        [stores.acache]
        type = "local"
        path = "./a"

        [stores.zshared]
        type = "local"
        path = "./z"

        [nodes.n]
        recipe = "r"
        target = true
        "#,
    )
    .unwrap();
    // primary comes first => it is the resolver's primary and rank 0
    let dag = bundle.raw.ingest().unwrap();
    assert_eq!(dag.primary, sn("zshared"));
    assert_eq!(dag.stores.get_index(0).unwrap().0, &sn("zshared"));
}

#[test]
fn ambiguous_primary_is_rejected() {
    let err = parse_toml(
        r#"
        [stores.a]
        type = "local"
        path = "./a"

        [stores.b]
        type = "local"
        path = "./b"

        [nodes.n]
        recipe = "r"
        target = true
        "#,
    )
    .unwrap_err();
    assert!(matches!(err, DagError::Value(msg) if msg.contains("primary")));
}

#[test]
fn remotes_variants_parse() {
    let bundle = parse_toml(
        r#"
        [stores.primary]
        type = "local"
        path = "./store"

        [stores.up]
        type = "remote"
        url = "https://example.org/store"

        [nodes.a]
        recipe = "r"
        remotes = "none"

        [nodes.b]
        recipe = "r"
        remotes = { allow = ["up"] }
        target = true
        [nodes.b.inputs]
        a = "a"
        "#,
    )
    .unwrap();
    assert_eq!(bundle.raw.nodes[0].remotes, ValidRemoteStores::None);
    assert_eq!(
        bundle.raw.nodes[1].remotes,
        ValidRemoteStores::Allow(vec![sn("up")])
    );
    assert!(matches!(
        bundle.locations[&sn("up")],
        xin_dag::StoreLocation::Remote { ref url } if url.ends_with("/store")
    ));
}

#[test]
fn bad_values_are_rejected() {
    let base = |extra: &str| {
        format!(
            r#"
            [stores.primary]
            type = "local"
            path = "./store"

            [nodes.n]
            recipe = "r"
            target = true
            {extra}
            "#
        )
    };
    assert!(matches!(
        parse_toml(&base("builder = \"magic\"")),
        Err(DagError::Value(_))
    ));
    assert!(matches!(
        parse_toml(&base("cores = \"seven\"")),
        Err(DagError::Value(_))
    ));
    assert!(matches!(
        parse_toml(&base("remotes = \"some\"")),
        Err(DagError::Value(_))
    ));
    // unknown fields in the intermediary are typos, not extensions
    assert!(matches!(
        parse_toml(&base("colour = \"red\"")),
        Err(DagError::Toml(_))
    ));
}

fn write_ncl(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    p
}

#[test]
fn nickel_generated_fanout_resolves() {
    // the point of a *generator*: a worker fan-out computed in Nickel, with
    // interpolation, folds, and the shipped contract filling defaults
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nickel_fanout");
    let file = write_ncl(
        &dir,
        "pipeline.ncl",
        r#"
let xin = import "xin.ncl" in
let worker_count = 3 in
let worker_ids = std.array.range 0 worker_count in
let name = fun i => "worker-%{std.string.from_number i}" in
let workers =
  std.array.fold_left
    (fun acc i =>
      std.record.insert
        (name i)
        {
          recipe = "process shard %{std.string.from_number i}",
          inputs.base = "base",
        }
        acc)
    {}
    worker_ids
in
let collect_inputs =
  std.array.fold_left
    (fun acc i => std.record.insert "w%{std.string.from_number i}" (name i) acc)
    {}
    worker_ids
in
{
  stores.primary = xin.local_store "./store",
  nodes =
    workers
    & {
      base = { recipe = "produce the dataset" },
      collect = {
        recipe = "merge the shards",
        inputs = collect_inputs,
        target = true,
      },
    },
} | xin.Dag
"#,
    );

    let (bundle, toml_text) = load(&file).unwrap();
    // the TOML intermediary is real interchange: it re-parses to the same DAG
    assert!(toml_text.contains("worker-2"), "intermediary:\n{toml_text}");
    parse_toml(&toml_text).unwrap();

    let dag = bundle.raw.ingest().unwrap();
    assert_eq!(dag.nodes.len(), 5); // base + 3 workers + collect
    let mut resolver = Resolver::new(dag);
    let mut world = SimWorld::new(&resolver.dag);
    let out = drive(&mut resolver, &mut world, 11);
    assert!(out.success, "statuses: {:?}", out.statuses);
    assert_eq!(world.builds_run, 5);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn nickel_contract_violation_reports_the_field() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nickel_bad");
    let file = write_ncl(
        &dir,
        "bad.ncl",
        r#"
let xin = import "xin.ncl" in
{
  stores.primary = xin.local_store "./store",
  nodes.broken = { target = true }, # no recipe
} | xin.Dag
"#,
    );
    let err = load(&file).unwrap_err();
    let DagError::Nickel(msg) = err else {
        panic!("expected a nickel error, got {err:?}")
    };
    assert!(
        msg.contains("recipe"),
        "diagnostics should name the missing field:\n{msg}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------- config

use xin_dag::config::{OnFailure, XinConfig};

fn write_config(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
    let p = dir.join("xin.config.toml");
    std::fs::write(&p, text).unwrap();
    p
}

#[test]
fn config_loads_stores_policy_and_absolutizes_paths() {
    let dir = std::env::temp_dir().join(format!("xin-cfg-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = write_config(
        &dir,
        "on_failure = \"fail-fast\"\nresults = \"out\"\n[stores.main]\ntype = \"local\"\npath = \"./store\"\n",
    );
    let cfg = XinConfig::load(&p).unwrap();
    assert_eq!(cfg.on_failure, OnFailure::FailFast);
    assert!(cfg.results.is_absolute() && cfg.results.ends_with("out"));
    let xin_dag::schema::TomlStore::Local { path, .. } = &cfg.stores["main"] else {
        panic!("expected a local store");
    };
    assert!(
        std::path::Path::new(path).is_absolute(),
        "store path {path} not absolutized"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_rejects_unknown_policy_words_and_fields() {
    let dir = std::env::temp_dir().join(format!("xin-cfg-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = write_config(&dir, "on_failure = \"explode\"\n");
    let err = XinConfig::load(&p).unwrap_err().to_string();
    assert!(err.contains("keep-going"), "{err}");
    let p = write_config(&dir, "no_such_setting = 1\n");
    assert!(
        XinConfig::load(&p).is_err(),
        "unknown fields must be rejected"
    );
    let p = write_config(&dir, "container = \"starship\"\n");
    let err = XinConfig::load(&p).unwrap_err().to_string();
    assert!(err.contains("bwrap"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn config_discovery_walks_upward() {
    let dir = std::env::temp_dir().join(format!("xin-cfg-find-{}", std::process::id()));
    let nested = dir.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(XinConfig::find(&nested), None);
    let p = write_config(&dir, "");
    assert_eq!(XinConfig::find(&nested), Some(p));
    std::fs::remove_dir_all(&dir).unwrap();
}
