//! The `xin` command (design.md B9): one binary, subcommands, JSON as the
//! primary output of every command (`--format json`), human-readable as a
//! formatted subset of the same data.

mod load;
mod report;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;

use xin_dag::config::{OnFailure, XinConfig};
use xin_dag::schema::TomlStore;
use xin_driver::{ContainerMode, ContainerPref};
use xin_resolver::input::{BuilderType, Cores};
use xin_resolver::resolver::NodeStatus;
use xin_resolver::sim::Policy;

#[derive(Parser)]
#[command(
    name = "xin",
    version,
    about = "content-addressed scientific build system"
)]
struct Cli {
    /// config file (default: search xin.config.toml upward from the cwd)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value_t = Format::Human)]
    format: Format,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Subcommand)]
enum Cmd {
    /// Evaluate a DAG file and realize its targets
    Build {
        /// DAG file; `name` means name.xin.ncl; default: the unique *.xin.ncl here
        file: Option<String>,
        /// stop scheduling new work after the first failure
        #[arg(long, conflicts_with = "keep_going")]
        fail_fast: bool,
        /// keep building unaffected nodes after failures (the default)
        #[arg(long)]
        keep_going: bool,
        /// print the resolver trace to stderr
        #[arg(long)]
        trace: bool,
        /// skip results/ symlinks and gc-root registration
        #[arg(long)]
        no_link: bool,
        /// build isolation: auto (bwrap when available), bwrap, none
        #[arg(long, value_name = "MODE")]
        container: Option<String>,
    },
    /// Open an interactive container with a node and its runtime closure
    /// mounted at /xin/<output-hash> (realizes the node first if needed)
    Shell {
        node: String,
        file: Option<String>,
        /// do not mount the host /nix into the container
        #[arg(long)]
        no_nix: bool,
        /// command to run instead of an interactive bash (after `--`)
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// What is present, cached, or would need building — runs no builds
    Status {
        file: Option<String>,
        #[arg(long)]
        trace: bool,
    },
    /// Print the TOML intermediary a DAG file evaluates to
    Eval { file: Option<String> },
    /// Show the pruned, validated DAG
    Dag { file: Option<String> },
    /// Show the stored build log of a node
    Log { node: String, file: Option<String> },
    /// Remove store paths unreachable from results links and live leases
    Gc {
        /// report what would be deleted without deleting
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect the configured stores
    Store {
        #[command(subcommand)]
        cmd: StoreCmd,
    },
}

#[derive(Subcommand)]
enum StoreCmd {
    /// Outputs, mappings, and sizes per local store
    Ls,
    /// GC roots and leases per local store
    Roots,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("xin: {msg}");
            ExitCode::from(2)
        }
    }
}

fn emit<T: Serialize>(format: Format, json: &T, human: String) {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(json).unwrap()),
        Format::Human => print!("{human}"),
    }
}

fn run(cli: &Cli) -> Result<ExitCode, String> {
    let config = cli.config.as_deref();
    match &cli.cmd {
        Cmd::Build {
            file,
            fail_fast,
            keep_going,
            trace,
            no_link,
            container,
        } => cmd_build(
            cli.format,
            config,
            file.as_deref(),
            *fail_fast,
            *keep_going,
            *trace,
            *no_link,
            container.as_deref(),
        ),
        Cmd::Shell {
            node,
            file,
            no_nix,
            cmd,
        } => cmd_shell(config, file.as_deref(), node, *no_nix, cmd),
        Cmd::Status { file, trace } => cmd_status(cli.format, config, file.as_deref(), *trace),
        Cmd::Eval { file } => cmd_eval(cli.format, config, file.as_deref()),
        Cmd::Dag { file } => cmd_dag(cli.format, config, file.as_deref()),
        Cmd::Log { node, file } => cmd_log(cli.format, config, file.as_deref(), node),
        Cmd::Gc { dry_run } => cmd_gc(cli.format, config, *dry_run),
        Cmd::Store { cmd } => cmd_store(cli.format, config, cmd),
    }
}

// ---------------------------------------------------------------- build

#[derive(Serialize)]
struct ResultLink {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct BuildReport {
    success: bool,
    policy: String,
    /// how builds were isolated: "bwrap" or "none"
    container: String,
    builds_run: u32,
    nodes: Vec<report::NodeReport>,
    results: Vec<ResultLink>,
}

/// CLI flag beats config beats "auto"; then resolve against the host.
fn container_mode(flag: Option<&str>, config: Option<&XinConfig>) -> Result<ContainerMode, String> {
    let word = flag
        .or(config.map(|c| c.container.as_str()))
        .unwrap_or("auto");
    let pref = ContainerPref::parse(word).ok_or_else(|| {
        format!("container must be \"auto\", \"bwrap\" or \"none\", found {word:?}")
    })?;
    ContainerMode::detect(pref).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    format: Format,
    config_arg: Option<&std::path::Path>,
    file: Option<&str>,
    fail_fast: bool,
    keep_going: bool,
    trace: bool,
    no_link: bool,
    container: Option<&str>,
) -> Result<ExitCode, String> {
    let config = load::load_config(config_arg)?;
    let loaded = load::load_bundle(file, config.as_ref())?;
    let mut driver = load::make_driver(&loaded.bundle).map_err(|e| e.to_string())?;
    driver.container = container_mode(container, config.as_ref())?;
    let mut resolver = load::make_resolver(&loaded)?;
    resolver.trace.enabled = trace;

    let policy = if fail_fast {
        Policy::FailFast
    } else if keep_going {
        Policy::KeepGoing
    } else {
        match config.as_ref().map(|c| c.on_failure) {
            Some(OnFailure::FailFast) => Policy::FailFast,
            _ => Policy::KeepGoing,
        }
    };

    let out = xin_driver::run_with_policy(&mut resolver, &mut driver, policy)
        .map_err(|e| format!("io error while building: {e}"))?;
    if trace {
        eprint!("{}", resolver.trace_report());
    }

    // A7: symlink realized targets into results/ under their human names,
    // and register each link as an indirect gc root
    let mut results = Vec::new();
    if !no_link {
        let results_dir = match &config {
            Some(c) => c.results.clone(),
            None => std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join("results"),
        };
        let dag = &resolver.dag;
        for &t in &dag.targets {
            let NodeStatus::Realized { output, store } = &out.statuses[t.idx()] else {
                continue;
            };
            std::fs::create_dir_all(&results_dir).map_err(|e| e.to_string())?;
            let link = results_dir.join(dag.nodes[t.idx()].name.as_str());
            let target = driver.local(store).output_dir(*output);
            match link.symlink_metadata() {
                Ok(m) if m.is_symlink() => {
                    std::fs::remove_file(&link).map_err(|e| e.to_string())?
                }
                Ok(_) => {
                    return Err(format!(
                        "{} exists and is not a symlink; not touching it",
                        link.display()
                    ));
                }
                Err(_) => {}
            }
            std::os::unix::fs::symlink(&target, &link).map_err(|e| e.to_string())?;
            driver
                .local(store)
                .register_root(&link)
                .map_err(|e| format!("registering gc root for {}: {e}", link.display()))?;
            results.push(ResultLink {
                name: dag.nodes[t.idx()].name.as_str().to_owned(),
                path: link.display().to_string(),
            });
        }
    }

    let nodes = report::node_reports(&out, &resolver.dag, false);
    let rep = BuildReport {
        success: out.success,
        policy: match policy {
            Policy::KeepGoing => "keep-going".into(),
            Policy::FailFast => "fail-fast".into(),
        },
        container: driver.container.name().into(),
        builds_run: driver.builds_run,
        nodes,
        results,
    };
    let mut human = report::render_nodes(&rep.nodes);
    for r in &rep.results {
        human.push_str(&format!("→ {}\n", r.path));
    }
    human.push_str(&format!(
        "{}: {} build{} run\n",
        if rep.success { "ok" } else { "FAILED" },
        rep.builds_run,
        if rep.builds_run == 1 { "" } else { "s" },
    ));
    emit(format, &rep, human);
    Ok(if rep.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// --------------------------------------------------------------- status

#[derive(Serialize)]
struct StatusReport {
    /// every target already realized — nothing to do
    up_to_date: bool,
    nodes: Vec<report::NodeReport>,
}

fn cmd_status(
    format: Format,
    config_arg: Option<&std::path::Path>,
    file: Option<&str>,
    trace: bool,
) -> Result<ExitCode, String> {
    let config = load::load_config(config_arg)?;
    let loaded = load::load_bundle(file, config.as_ref())?;
    let mut driver = load::make_driver(&loaded.bundle).map_err(|e| e.to_string())?;
    driver.query_only = true;
    let mut resolver = load::make_resolver(&loaded)?;
    resolver.trace.enabled = trace;
    let out = xin_driver::run_to_quiescence(&mut resolver, &mut driver)
        .map_err(|e| format!("io error while querying: {e}"))?;
    if trace {
        eprint!("{}", resolver.trace_report());
    }
    let rep = StatusReport {
        up_to_date: out.success,
        nodes: report::node_reports(&out, &resolver.dag, true),
    };
    let mut human = report::render_nodes(&rep.nodes);
    human.push_str(if rep.up_to_date {
        "up to date\n"
    } else {
        "not up to date\n"
    });
    emit(format, &rep, human);
    Ok(ExitCode::SUCCESS)
}

// ----------------------------------------------------------- eval / dag

fn cmd_eval(
    format: Format,
    _config_arg: Option<&std::path::Path>,
    file: Option<&str>,
) -> Result<ExitCode, String> {
    let path = load::resolve_dag_path(file)?;
    let (_, text) = xin_dag::load_dag(&path).map_err(|e| e.to_string())?;
    match format {
        Format::Human => print!("{text}"),
        Format::Json => {
            // same data, JSON syntax (B10): the intermediary is a value
            let value: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct DagNodeReport {
    id: u32,
    name: String,
    builder: String,
    target: bool,
    cores: String,
    target_store: String,
    /// alias -> upstream node name
    inputs: Vec<(String, String)>,
}

#[derive(Serialize)]
struct DagStoreReport {
    name: String,
    kind: String,
    primary: bool,
}

#[derive(Serialize)]
struct DagReport {
    file: String,
    targets: Vec<String>,
    nodes: Vec<DagNodeReport>,
    stores: Vec<DagStoreReport>,
}

fn cmd_dag(
    format: Format,
    config_arg: Option<&std::path::Path>,
    file: Option<&str>,
) -> Result<ExitCode, String> {
    let config = load::load_config(config_arg)?;
    let loaded = load::load_bundle(file, config.as_ref())?;
    let resolver = load::make_resolver(&loaded)?;
    let dag = &resolver.dag;
    let rep = DagReport {
        file: loaded.path.display().to_string(),
        targets: dag
            .targets
            .iter()
            .map(|t| dag.nodes[t.idx()].name.as_str().to_owned())
            .collect(),
        nodes: dag
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| DagNodeReport {
                id: i as u32,
                name: n.name.as_str().to_owned(),
                builder: match n.builder {
                    BuilderType::Process => "process".into(),
                    BuilderType::FetchUrl => "fetchurl".into(),
                },
                target: n.is_target,
                cores: match n.cores {
                    Cores::One => "one".into(),
                    Cores::All => "all".into(),
                },
                target_store: n.target_store.to_string(),
                inputs: n
                    .upstreams
                    .iter()
                    .map(|(alias, up)| {
                        (
                            alias.as_str().to_owned(),
                            dag.nodes[up.idx()].name.as_str().to_owned(),
                        )
                    })
                    .collect(),
            })
            .collect(),
        stores: dag
            .stores
            .iter()
            .map(|(name, def)| DagStoreReport {
                name: name.to_string(),
                kind: match def {
                    xin_resolver::input::StoreDef::Local { writeable: true } => "local".into(),
                    xin_resolver::input::StoreDef::Local { writeable: false } => {
                        "local (read-only)".into()
                    }
                    xin_resolver::input::StoreDef::Remote => "remote".into(),
                },
                primary: *name == dag.primary,
            })
            .collect(),
    };
    let mut human = format!(
        "{} — {} node(s), pruned to targets\n",
        rep.file,
        rep.nodes.len()
    );
    for n in &rep.nodes {
        let deps = if n.inputs.is_empty() {
            String::new()
        } else {
            format!(
                "  ← {}",
                n.inputs
                    .iter()
                    .map(|(a, u)| if a == u {
                        u.clone()
                    } else {
                        format!("{a}={u}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        human.push_str(&format!(
            "{:3} {}{}{}\n",
            n.id,
            n.name,
            if n.target { " [target]" } else { "" },
            deps
        ));
    }
    for s in &rep.stores {
        human.push_str(&format!(
            "store {} ({}){}\n",
            s.name,
            s.kind,
            if s.primary { " [primary]" } else { "" }
        ));
    }
    emit(format, &rep, human);
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ log

#[derive(Serialize)]
struct LogReport {
    node: String,
    input_hash: String,
    found_in: String,
    exit: i32,
    stdout: String,
    stderr: String,
}

fn cmd_log(
    format: Format,
    config_arg: Option<&std::path::Path>,
    file: Option<&str>,
    node: &str,
) -> Result<ExitCode, String> {
    let config = load::load_config(config_arg)?;
    let loaded = load::load_bundle(file, config.as_ref())?;
    let mut driver = load::make_driver(&loaded.bundle).map_err(|e| e.to_string())?;
    driver.query_only = true;
    let mut resolver = load::make_resolver(&loaded)?;
    xin_driver::run_to_quiescence(&mut resolver, &mut driver)
        .map_err(|e| format!("io error while querying: {e}"))?;
    let id = resolver
        .dag
        .id_of(node)
        .ok_or_else(|| format!("no node named {node} in {}", loaded.path.display()))?;
    let ih = resolver.nodes[id.idx()].input_hash.ok_or_else(|| {
        format!("{node} has no input-hash yet (upstreams not named); nothing was built")
    })?;
    for (store_name, _) in &loaded.bundle.raw.stores {
        let Some(xin_dag::StoreLocation::Local { path, .. }) =
            loaded.bundle.locations.get(store_name)
        else {
            continue;
        };
        let store = xin_driver::LocalStore::open(path).map_err(|e| e.to_string())?;
        if let Some(log) = store.read_build_meta(ih).map_err(|e| e.to_string())? {
            let rep = LogReport {
                node: node.to_owned(),
                input_hash: ih.to_string(),
                found_in: store_name.to_string(),
                exit: log.return_code,
                stdout: String::from_utf8_lossy(&log.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&log.stderr).into_owned(),
            };
            let human = format!(
                "{} ({})\nbuilt in store {} — exit {}\n--- stdout ---\n{}--- stderr ---\n{}",
                rep.node, rep.input_hash, rep.found_in, rep.exit, rep.stdout, rep.stderr
            );
            emit(format, &rep, human);
            return Ok(ExitCode::SUCCESS);
        }
    }
    Err(format!("no build log for {node} ({ih}) in any local store"))
}

// ------------------------------------------------------------------- gc

#[derive(Serialize)]
struct GcStoreReport {
    store: String,
    path: String,
    live_outputs: usize,
    deleted_outputs: Vec<String>,
    freed_bytes: u64,
    pruned_inputs: usize,
    pruned_meta: usize,
    cleaned_temp: usize,
    dropped_roots: usize,
    dropped_leases: usize,
    kept_roots: usize,
    kept_leases: usize,
}

#[derive(Serialize)]
struct GcRunReport {
    dry_run: bool,
    stores: Vec<GcStoreReport>,
}

fn cmd_gc(
    format: Format,
    config_arg: Option<&std::path::Path>,
    dry_run: bool,
) -> Result<ExitCode, String> {
    let cfg = load::require_config(config_arg)?;
    let stores = load::open_config_stores(&cfg)?;
    let refs: Vec<&xin_driver::LocalStore> = stores.iter().map(|(_, s)| s).collect();
    let reports = xin_driver::run_gc(&refs, dry_run).map_err(|e| format!("gc: {e}"))?;
    let rep = GcRunReport {
        dry_run,
        stores: stores
            .iter()
            .zip(reports)
            .map(|((name, store), r)| GcStoreReport {
                store: name.clone(),
                path: store.root.display().to_string(),
                live_outputs: r.live_outputs,
                deleted_outputs: r.deleted_outputs.iter().map(|o| o.to_string()).collect(),
                freed_bytes: r.freed_bytes,
                pruned_inputs: r.pruned_inputs,
                pruned_meta: r.pruned_meta,
                cleaned_temp: r.cleaned_temp,
                dropped_roots: r.dropped_roots,
                dropped_leases: r.dropped_leases,
                kept_roots: r.kept_roots,
                kept_leases: r.kept_leases,
            })
            .collect(),
    };
    let mut human = String::new();
    for s in &rep.stores {
        human.push_str(&format!(
            "{} ({}): {} live, {}{} output(s) removed ({} bytes), {} mapping(s) and {} log(s) pruned, {} temp dir(s) cleaned\n",
            s.store,
            s.path,
            s.live_outputs,
            if rep.dry_run { "would be: " } else { "" },
            s.deleted_outputs.len(),
            s.freed_bytes,
            s.pruned_inputs,
            s.pruned_meta,
            s.cleaned_temp,
        ));
        if s.dropped_roots + s.dropped_leases > 0 {
            human.push_str(&format!(
                "  dropped {} stale root(s), {} dead lease(s); kept {} root(s), {} lease(s)\n",
                s.dropped_roots, s.dropped_leases, s.kept_roots, s.kept_leases
            ));
        }
    }
    emit(format, &rep, human);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- store

#[derive(Serialize)]
struct StoreLsOutput {
    output: String,
    bytes: u64,
}

#[derive(Serialize)]
struct StoreLsReport {
    store: String,
    path: String,
    outputs: Vec<StoreLsOutput>,
    mappings: usize,
    roots: Vec<(String, String)>,
    leases: usize,
    temp_dirs: usize,
}

fn cmd_store(
    format: Format,
    config_arg: Option<&std::path::Path>,
    cmd: &StoreCmd,
) -> Result<ExitCode, String> {
    let cfg = load::require_config(config_arg)?;
    let mut reports = Vec::new();
    for (name, def) in &cfg.stores {
        let TomlStore::Local { path, .. } = def else {
            continue;
        };
        let store = xin_driver::LocalStore::open(path).map_err(|e| e.to_string())?;
        let inv = xin_driver::gc::inventory(&store).map_err(|e| e.to_string())?;
        reports.push(StoreLsReport {
            store: name.clone(),
            path: path.clone(),
            outputs: inv
                .outputs
                .iter()
                .map(|(oh, bytes)| StoreLsOutput {
                    output: oh.to_string(),
                    bytes: *bytes,
                })
                .collect(),
            mappings: inv.mappings,
            roots: inv.roots,
            leases: inv.leases,
            temp_dirs: inv.temp_dirs,
        });
    }
    let mut human = String::new();
    match cmd {
        StoreCmd::Ls => {
            for s in &reports {
                human.push_str(&format!(
                    "{} ({}): {} output(s), {} mapping(s)\n",
                    s.store,
                    s.path,
                    s.outputs.len(),
                    s.mappings
                ));
                for o in &s.outputs {
                    human.push_str(&format!("  {}  {} bytes\n", o.output, o.bytes));
                }
            }
        }
        StoreCmd::Roots => {
            for s in &reports {
                human.push_str(&format!(
                    "{} ({}): {} root(s), {} lease(s), {} temp dir(s)\n",
                    s.store,
                    s.path,
                    s.roots.len(),
                    s.leases,
                    s.temp_dirs
                ));
                for (entry, target) in &s.roots {
                    human.push_str(&format!("  {entry} → {target}\n"));
                }
            }
        }
    }
    emit(format, &reports, human);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- shell

/// Realize one node (whatever the DAG's own targets say), then drop the
/// user into a bwrap container whose /xin holds the node's entire runtime
/// closure at the canonical relocatable places — the same view a build
/// gets, plus network, the host cwd at /xin/work, and (by default) the
/// host /nix so jupyter & friends are available (B8).
fn cmd_shell(
    config_arg: Option<&std::path::Path>,
    file: Option<&str>,
    node: &str,
    no_nix: bool,
    cmd: &[String],
) -> Result<ExitCode, String> {
    let config = load::load_config(config_arg)?;
    let mut loaded = load::load_bundle(file, config.as_ref())?;

    // the requested node is the only target of this run: prune to its
    // closure, realize exactly it
    let mut found = false;
    for (name, raw_node) in loaded.bundle.raw.nodes.iter_mut() {
        raw_node.is_target = name.as_str() == node;
        found |= raw_node.is_target;
    }
    if !found {
        return Err(format!("no node named {node} in {}", loaded.path.display()));
    }

    let ContainerMode::Bwrap { bwrap } =
        ContainerMode::detect(ContainerPref::Bwrap).map_err(|e| e.to_string())?
    else {
        unreachable!("ContainerPref::Bwrap never resolves to Direct");
    };

    let mut driver = load::make_driver(&loaded.bundle).map_err(|e| e.to_string())?;
    driver.container = container_mode(None, config.as_ref())?;
    let mut resolver = load::make_resolver(&loaded)?;
    let out = xin_driver::run_to_quiescence(&mut resolver, &mut driver)
        .map_err(|e| format!("io error while realizing {node}: {e}"))?;
    let id = resolver.dag.id_of(node).expect("target survives pruning");
    let NodeStatus::Realized { output, .. } = &out.statuses[id.idx()] else {
        let reports = report::node_reports(&out, &resolver.dag, false);
        return Err(format!(
            "could not realize {node}:\n{}",
            report::render_nodes(&reports)
        ));
    };

    // mount set: the on-disk runtime closure, wherever its members live
    let stores: Vec<xin_driver::LocalStore> = loaded
        .bundle
        .locations
        .values()
        .filter_map(|loc| match loc {
            xin_dag::StoreLocation::Local { path, .. } => xin_driver::LocalStore::open(path).ok(),
            xin_dag::StoreLocation::Remote { .. } => None,
        })
        .collect();
    let refs: Vec<&xin_driver::LocalStore> = stores.iter().collect();
    let closure = xin_driver::runtime_closure(&refs, *output);

    let mut bw = xin_driver::Bwrap::new(&bwrap, true); // interactive: network stays
    bw.host_toolchain(!no_nix);
    let mut node_dir = None;
    for oh in &closure {
        let dir = refs
            .iter()
            .map(|s| s.output_dir(*oh))
            .find(|d| d.is_dir())
            .ok_or_else(|| {
                format!("runtime closure member {oh} is in no local store (gc'd? run xin build)")
            })?;
        if oh == output {
            node_dir = Some(dir.clone());
        }
        bw.ro_bind(&dir, format!("/xin/{oh}"));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    bw.bind(&cwd, "/xin/work");
    bw.chdir("/xin/work");
    bw.setenv("XIN_NODE", format!("/xin/{output}"));
    bw.setenv("HOME", "/xin/work");
    if let Ok(term) = std::env::var("TERM") {
        bw.setenv("TERM", term);
    }
    // the node's own executables first on PATH, if it ships any
    if node_dir.is_some_and(|d| d.join("payload").join("bin").is_dir()) {
        let host_path = std::env::var("PATH").unwrap_or_default();
        bw.setenv("PATH", format!("/xin/{output}/payload/bin:{host_path}"));
    }

    let argv: Vec<&str> = if cmd.is_empty() {
        vec!["bash"]
    } else {
        cmd.iter().map(String::as_str).collect()
    };
    eprintln!(
        "xin shell: {node} at /xin/{output} ({} output(s) mounted, {}, cwd → /xin/work)",
        closure.len(),
        if no_nix { "no /nix" } else { "/nix mounted" },
    );
    let status = bw
        .command(&argv)
        .status()
        .map_err(|e| format!("launching container: {e}"))?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}
