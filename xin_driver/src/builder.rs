//! The Process builder: stage a B4-shaped build dir in temp/, then run the
//! recipe in the mandatory sandbox (see `container`) with the staged dirs
//! bound at the canonical B4 places:
//!
//! ```text
//! <build>/recipe.sh                     -> /xin/recipe.sh          (ro)
//! <build>/bootstrap/                    -> /xin/bootstrap  (PATH)  (ro)
//! <build>/inputs/by-name/<alias>        -> /xin/inputs/by-name     (ro)
//!                                          (relative ../../<oh> symlinks)
//! <build>/out/{payload,runtime-inputs}/ -> /xin/out                (rw)
//! <build>/work/                         -> /xin/work  (cwd, HOME)  (rw)
//! each input <store>/outputs/<sh>/<oh>  -> /xin/<oh>               (ro)
//! ```
//!
//! Recipes execute as `/xin/bootstrap/sh /xin/recipe.sh` — busybox ash,
//! not bash. Anything richer is a declared input:
//! `exec "$XIN_INPUTS/bash/payload/bin/bash" ...`.
//!
//! Environment: `XIN_OUT`, `XIN_PAYLOAD`, `XIN_INPUTS`, and per input
//! `XIN_INPUT_HASH_<ALIAS>` ('-' mapped to '_') so a script can declare a
//! runtime dependency with the canonical A1 link content:
//!
//! ```sh
//! ln -s "../../$XIN_INPUT_HASH_a" "$XIN_OUT/runtime-inputs/a"
//! ```
//!
//! Runtime refs are *read back* from out/runtime-inputs (B2: explicit
//! declaration, no output scanning), which also lints the link form (B1).

use std::io;
use std::path::PathBuf;

use xin_resolver::events::BuildOutcome;
use xin_resolver::failure::BuildLog;
use xin_resolver::hashes::{InputHash, OutputHash};
use xin_resolver::input::InputName;

use crate::container::{Bwrap, Sandbox};
use crate::local_store::{LocalStore, read_runtime_refs};
use crate::tree_hash::hash_tree;

/// On success, the staged tree (`<build>/out`) is returned for the ack-gated
/// commit; the build dir around it is the driver's to clean up. On build
/// failure the whole build dir is left in temp/ for inspection (B5: failed
/// builds stick around).
pub struct BuildRun {
    pub outcome: BuildOutcome,
    pub build_dir: PathBuf,
    pub staged_out: Option<PathBuf>,
}

pub fn run_process_build(
    store: &mut LocalStore,
    input: InputHash,
    recipe: &[u8],
    inputs: &[(InputName, OutputHash)],
    find_input: impl Fn(OutputHash) -> Option<PathBuf>,
    sandbox: &Sandbox,
) -> io::Result<BuildRun> {
    let ih_short = &input.to_string()[..8];
    let dir = store.new_temp_dir(&format!("build-{ih_short}"))?;
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("payload"))?;
    std::fs::create_dir_all(out.join("runtime-inputs"))?;
    let work = dir.join("work");
    std::fs::create_dir_all(&work)?;
    let by_name = dir.join("inputs").join("by-name");
    std::fs::create_dir_all(&by_name)?;
    let boot = sandbox.stage_bootstrap(&dir)?;
    let script = dir.join("recipe.sh");
    std::fs::write(&script, recipe)?;

    let mut bw = Bwrap::new(&sandbox.bwrap, false); // B3: no network
    let mut bound = std::collections::BTreeSet::new();
    for (alias, oh) in inputs {
        let src = find_input(*oh).ok_or_else(|| {
            io::Error::other(format!(
                "input {oh:?} for alias {alias} is not in any local store"
            ))
        })?;
        // the alias link carries the canonical relocatable form (B4); it
        // resolves against /xin, not against the host store layout
        std::os::unix::fs::symlink(format!("../../{oh}"), by_name.join(alias.as_str()))?;
        if bound.insert(*oh) {
            bw.ro_bind(src, format!("/xin/{oh}"));
        }
        bw.setenv(
            &format!("XIN_INPUT_HASH_{}", alias.as_str().replace('-', "_")),
            oh.to_string(),
        );
    }
    bw.bind(&out, "/xin/out");
    bw.ro_bind(dir.join("inputs"), "/xin/inputs");
    bw.ro_bind(&boot, "/xin/bootstrap");
    bw.ro_bind(&script, "/xin/recipe.sh");
    bw.bind(&work, "/xin/work");
    bw.chdir("/xin/work");
    bw.setenv("PATH", "/xin/bootstrap");
    bw.setenv("XIN_OUT", "/xin/out");
    bw.setenv("XIN_PAYLOAD", "/xin/out/payload");
    bw.setenv("XIN_INPUTS", "/xin/inputs/by-name");
    bw.setenv("HOME", "/xin/work");
    let mut cmd = bw.command(&["/xin/bootstrap/sh", "/xin/recipe.sh"]);
    let result = cmd.output()?; // no timeout yet: prototype builder
    let log = BuildLog {
        stdout: result.stdout,
        stderr: result.stderr,
        return_code: result.status.code().unwrap_or(-1),
    };
    finish_build(dir, out, log, result.status.success())
}

/// The shared back half of every builder: read the declared refs back
/// (B2/B1 lint) and tree-hash the staged output into its name (B12).
fn finish_build(dir: PathBuf, out: PathBuf, log: BuildLog, ran_ok: bool) -> io::Result<BuildRun> {
    if !ran_ok {
        return Ok(BuildRun {
            outcome: BuildOutcome::Failure { log },
            build_dir: dir,
            staged_out: None,
        });
    }
    let runtime_refs = match read_runtime_refs(&out) {
        Ok(refs) => refs,
        Err(e) => {
            // declared refs are malformed: a build failure, not an IO error
            let log = BuildLog {
                stdout: log.stdout,
                stderr: format!("{}\nxin: {e}", String::from_utf8_lossy(&log.stderr)).into_bytes(),
                return_code: -2,
            };
            return Ok(BuildRun {
                outcome: BuildOutcome::Failure { log },
                build_dir: dir,
                staged_out: None,
            });
        }
    };
    let output = hash_tree(&out)?;
    Ok(BuildRun {
        outcome: BuildOutcome::Success {
            output,
            runtime_refs,
            log,
        },
        build_dir: dir,
        staged_out: Some(out),
    })
}

/// The FetchUrl builder (A2): recipe = a URL, output = payload/<basename>,
/// no runtime refs. `file://` copies from the host — a fetcher's whole job
/// is to bring the outside world in, and TOFU (the resolver pins the first
/// fetch's output-hash) is the guard rail, not the sandbox. `http(s)://`
/// runs busybox wget in a *network-enabled* sandbox (fixed-output nodes
/// are the one class allowed network, B3), with only resolv.conf and the
/// CA bundle from the host.
pub fn run_fetch_url(
    store: &mut LocalStore,
    input: InputHash,
    recipe: &[u8],
    sandbox: &Sandbox,
) -> io::Result<BuildRun> {
    let ih_short = &input.to_string()[..8];
    let dir = store.new_temp_dir(&format!("fetch-{ih_short}"))?;
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("payload"))?;
    std::fs::create_dir_all(out.join("runtime-inputs"))?;
    let url = String::from_utf8_lossy(recipe).trim().to_string();
    let name = fetch_basename(&url);
    let dest = out.join("payload").join(&name);

    let fail = |dir: PathBuf, msg: String| {
        Ok(BuildRun {
            outcome: BuildOutcome::Failure {
                log: BuildLog {
                    stdout: Vec::new(),
                    stderr: msg.into_bytes(),
                    return_code: -1,
                },
            },
            build_dir: dir,
            staged_out: None,
        })
    };

    if let Some(path) = url.strip_prefix("file://") {
        return match std::fs::copy(path, &dest) {
            Ok(_) => finish_build(
                dir,
                out,
                BuildLog {
                    stdout: format!("fetched {url} -> payload/{name}\n").into_bytes(),
                    stderr: Vec::new(),
                    return_code: 0,
                },
                true,
            ),
            Err(e) => fail(dir, format!("xin: fetch {url}: {e}")),
        };
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return fail(
            dir,
            format!("xin: fetchurl supports file://, http:// and https://, got {url:?}"),
        );
    }

    let boot = sandbox.stage_bootstrap(&dir)?;
    let mut bw = Bwrap::new(&sandbox.bwrap, true); // fixed-output: network allowed
    bw.bind(&out, "/xin/out");
    bw.ro_bind(&boot, "/xin/bootstrap");
    bw.ro_bind_try("/etc/resolv.conf", "/etc/resolv.conf");
    bw.ro_bind_try("/etc/ssl", "/etc/ssl");
    bw.setenv("PATH", "/xin/bootstrap");
    bw.setenv("HOME", "/tmp");
    let mut cmd = bw.command(&[
        "/xin/bootstrap/wget",
        "-O",
        &format!("/xin/out/payload/{name}"),
        &url,
    ]);
    let result = cmd.output()?;
    let log = BuildLog {
        stdout: result.stdout,
        stderr: result.stderr,
        return_code: result.status.code().unwrap_or(-1),
    };
    if !result.status.success() {
        // wget leaves an empty -O file behind on failure; drop it
        let _ = std::fs::remove_file(&dest);
    }
    finish_build(dir, out, log, result.status.success())
}

/// A deterministic payload filename from the URL's last path segment.
fn fetch_basename(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let seg = no_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let cleaned: String = seg
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "fetched".to_owned()
    } else {
        cleaned
    }
}
