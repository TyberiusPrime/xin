//! End-to-end tests of the `xin` binary: real Nickel evaluation, real
//! stores, real subprocess builds, driven through the CLI exactly as a
//! user would. Each test gets its own directory under the cargo target
//! tmpdir, kept (path printed) on failure, removed otherwise.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

    fn write(&self, name: &str, content: &str) {
        fs::write(self.path.join(name), content).unwrap();
    }

    fn xin(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xin"))
            .args(args)
            .current_dir(&self.path)
            .output()
            .expect("spawning xin")
    }

    /// Run, demand success, parse the JSON report.
    fn xin_json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = args.to_vec();
        full.extend(["--format", "json"]);
        let out = self.xin(&full);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "xin {args:?} did not print JSON ({e}):\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
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

const CONFIG: &str = r#"
[stores.main]
type = "local"
path = "./store"
"#;

/// base → top with a declared runtime ref, so gc's closure logic is
/// exercised (top rooted keeps base alive).
const CHAIN_DAG: &str = r#"
let xin = import "xin.ncl" in
{
  nodes = {
    base = { recipe = m%"echo hello > "$XIN_OUT/payload/greeting""% },
    top = {
      recipe = m%"
        test ! -e /nix
        tr a-z A-Z < "$XIN_INPUTS/base/payload/greeting" > "$XIN_OUT/payload/shout"
        ln -s "../../$XIN_INPUT_HASH_base" "$XIN_OUT/runtime-inputs/base"
      "%,
      inputs.base = "base",
      target = true,
    },
  },
} | xin.Dag
"#;

#[test]
fn build_status_rebuild_roundtrip() {
    let t = TestDir::new("cli_roundtrip");
    t.write("xin.config.toml", CONFIG);
    t.write("demo.xin.ncl", CHAIN_DAG);

    // before anything: status says both nodes are outstanding, runs nothing
    let st = t.xin_json(&["status", "demo"]);
    assert_eq!(st["up_to_date"], false);
    assert_eq!(st["nodes"][0]["status"], "needs-build");
    assert_eq!(st["nodes"][1]["status"], "blocked");

    // `demo` resolves to demo.xin.ncl (the default extension)
    let build = t.xin_json(&["build", "demo"]);
    assert_eq!(build["success"], true);
    assert_eq!(build["builds_run"], 2);
    assert_eq!(build["nodes"][1]["status"], "realized");

    // A7: the target is linked under its human name and actually works
    let shout = t.path.join("results/top/payload/shout");
    assert_eq!(fs::read_to_string(&shout).unwrap(), "HELLO\n");

    // second build: everything cached, zero builds, still success
    let again = t.xin_json(&["build", "demo"]);
    assert_eq!(again["success"], true);
    assert_eq!(again["builds_run"], 0);

    let st = t.xin_json(&["status", "demo"]);
    assert_eq!(st["up_to_date"], true);
    assert_eq!(st["nodes"][1]["status"], "present");
    // A1 early cutoff: with top present, base's bytes were never demanded —
    // status honestly says "named", not "present"
    assert_eq!(st["nodes"][0]["status"], "named");
}

#[test]
fn no_arg_finds_the_unique_dag_file() {
    let t = TestDir::new("cli_no_arg");
    t.write("xin.config.toml", CONFIG);
    t.write("only.xin.ncl", CHAIN_DAG);
    let build = t.xin_json(&["build"]);
    assert_eq!(build["success"], true);

    // a second candidate makes the bare invocation ambiguous
    t.write("other.xin.ncl", CHAIN_DAG);
    let out = t.xin(&["build"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("only.xin.ncl") && err.contains("other.xin.ncl"),
        "{err}"
    );
}

#[test]
fn failing_build_blame_and_log() {
    let t = TestDir::new("cli_failure");
    t.write("xin.config.toml", CONFIG);
    t.write(
        "fail.xin.ncl",
        r#"
let xin = import "xin.ncl" in
{
  nodes = {
    boom = { recipe = "echo diagnostic-output >&2; exit 3" },
    user = {
      recipe = m%"cp -r "$XIN_INPUTS/boom/payload" "$XIN_PAYLOAD/copy""%,
      inputs.boom = "boom",
      target = true,
    },
  },
} | xin.Dag
"#,
    );
    let out = t.xin(&["build", "fail", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "build failure exits 1");
    let rep: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rep["success"], false);
    assert_eq!(rep["nodes"][0]["failure"]["kind"], "Build");
    // the downstream node blames its upstream, root cause last
    let chain = rep["nodes"][1]["failure"]["chain"].as_array().unwrap();
    assert_eq!(chain.last().unwrap()["origin"], "boom");
    assert_eq!(chain.last().unwrap()["kind"], "Build");

    // the failed build's log is on disk and addressable by node name
    let log = t.xin_json(&["log", "boom", "fail"]);
    assert_eq!(log["exit"], 3);
    assert!(
        log["stderr"]
            .as_str()
            .unwrap()
            .contains("diagnostic-output")
    );
}

#[test]
fn gc_respects_results_roots_and_sweeps_after_release() {
    let t = TestDir::new("cli_gc");
    t.write("xin.config.toml", CONFIG);
    t.write("demo.xin.ncl", CHAIN_DAG);
    assert_eq!(t.xin_json(&["build", "demo"])["success"], true);

    // rooted: dry-run and real gc both keep the full closure (base lives
    // only through top's runtime ref)
    let gc = t.xin_json(&["gc", "--dry-run"]);
    assert_eq!(gc["stores"][0]["live_outputs"], 2);
    assert_eq!(
        gc["stores"][0]["deleted_outputs"].as_array().unwrap().len(),
        0
    );
    let gc = t.xin_json(&["gc"]);
    assert_eq!(
        gc["stores"][0]["deleted_outputs"].as_array().unwrap().len(),
        0
    );
    let ls = t.xin_json(&["store", "ls"]);
    assert_eq!(ls[0]["outputs"].as_array().unwrap().len(), 2);

    // deleting the results link releases the root; gc sweeps outputs,
    // mappings and logs
    fs::remove_file(t.path.join("results/top")).unwrap();
    let gc = t.xin_json(&["gc"]);
    assert_eq!(gc["stores"][0]["live_outputs"], 0);
    assert_eq!(
        gc["stores"][0]["deleted_outputs"].as_array().unwrap().len(),
        2
    );
    assert_eq!(gc["stores"][0]["pruned_inputs"], 2);
    assert_eq!(gc["stores"][0]["dropped_roots"], 1);
    let ls = t.xin_json(&["store", "ls"]);
    assert_eq!(ls[0]["outputs"].as_array().unwrap().len(), 0);

    // and the world is rebuildable from scratch
    let build = t.xin_json(&["build", "demo"]);
    assert_eq!(build["success"], true);
    assert_eq!(build["builds_run"], 2);
}

#[test]
fn eval_prints_the_intermediary_and_dag_shows_topology() {
    let t = TestDir::new("cli_eval");
    t.write("xin.config.toml", CONFIG);
    t.write("demo.xin.ncl", CHAIN_DAG);

    let out = t.xin(&["eval", "demo"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let parsed: toml::Value = toml::from_str(&text).expect("eval output is valid TOML");
    assert!(parsed["nodes"]["top"]["inputs"]["base"] == toml::Value::String("base".into()));

    let dag = t.xin_json(&["dag", "demo"]);
    assert_eq!(dag["targets"][0], "top");
    // topological interning: base (no inputs) before top
    assert_eq!(dag["nodes"][0]["name"], "base");
    assert_eq!(dag["nodes"][1]["inputs"][0][1], "base");
    assert_eq!(dag["stores"][0]["primary"], true);
}

#[test]
fn storeless_toml_dag_uses_config_stores() {
    let t = TestDir::new("cli_storeless_toml");
    t.write("xin.config.toml", CONFIG);
    // a hand-written TOML intermediary with no stores of its own
    t.write(
        "plain.toml",
        r#"
[nodes.solo]
recipe = 'echo made > "$XIN_OUT/payload/out"'
target = true
"#,
    );
    let build = t.xin_json(&["build", "plain.toml"]);
    assert_eq!(build["success"], true);
    assert!(t.path.join("store/outputs").is_dir());

    // without any config, the same file cannot resolve a primary store
    fs::remove_file(t.path.join("xin.config.toml")).unwrap();
    fs::remove_dir_all(t.path.join("results")).unwrap();
    let out = t.xin(&["build", "plain.toml"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("xin.config.toml"),
        "error should point at the config mechanism"
    );
}

#[test]
fn duplicate_store_definition_is_rejected() {
    let t = TestDir::new("cli_dup_store");
    t.write("xin.config.toml", CONFIG);
    t.write(
        "dup.xin.ncl",
        r#"
let xin = import "xin.ncl" in
{
  stores.main = xin.local_store "./elsewhere",
  nodes.solo = { recipe = "true", target = true },
} | xin.Dag
"#,
    );
    let out = t.xin(&["build", "dup"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("main") && err.contains("both"), "{err}");
}

fn have_bwrap() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bwrap").is_file()))
        .unwrap_or(false)
}

#[test]
fn shell_mounts_the_runtime_closure() {
    if !have_bwrap() {
        eprintln!("skipping shell test: no bwrap in PATH");
        return;
    }
    let t = TestDir::new("cli_shell");
    t.write("xin.config.toml", CONFIG);
    t.write("demo.xin.ncl", CHAIN_DAG);

    // shell realizes the node itself (no prior xin build), then the
    // in-container view resolves runtime refs through /xin relative links
    let script = format!(
        r#"set -e
cat "$XIN_NODE/payload/shout"
cat "$XIN_NODE/runtime-inputs/base/payload/greeting"
test -d /nix
test "$PWD" = /xin/work
test ! -e "{host}"
"#,
        host = t.path.join("store").display()
    );
    let out = t.xin(&["shell", "top", "demo", "--", "bash", "-c", &script]);
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "HELLO\nhello\n");

    // exit codes pass through
    let out = t.xin(&["shell", "top", "demo", "--", "bash", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7));

    // unknown node names are user errors
    let out = t.xin(&["shell", "nonesuch", "demo"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn shipped_examples_evaluate() {
    // bit-rot guard: every example's Nickel file must evaluate to a valid
    // TOML intermediary (building them is the READMEs' job)
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples");
    let mut seen = 0;
    for entry in fs::read_dir(&examples).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for f in fs::read_dir(&dir).unwrap() {
            let f = f.unwrap().path();
            if f.to_str().is_some_and(|p| p.ends_with(".xin.ncl")) {
                let out = Command::new(env!("CARGO_BIN_EXE_xin"))
                    .args(["eval", f.to_str().unwrap()])
                    .current_dir(&dir)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}: {}",
                    f.display(),
                    String::from_utf8_lossy(&out.stderr)
                );
                let text = String::from_utf8(out.stdout).unwrap();
                let parsed: toml::Value = toml::from_str(&text).unwrap();
                assert!(
                    parsed.get("nodes").is_some(),
                    "{}: intermediary has no nodes",
                    f.display()
                );
                seen += 1;
            }
        }
    }
    assert!(seen >= 4, "expected the shipped examples, found {seen}");
}
