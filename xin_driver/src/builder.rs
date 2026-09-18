//! The Process builder: stage a B4-shaped build dir in temp/, then run the
//! recipe either directly on the host (`ContainerMode::Direct`) or inside a
//! bubblewrap container (`ContainerMode::Bwrap`) with the staged dirs bound
//! at the canonical B4 places:
//!
//! ```text
//! <build>/recipe.sh                     -> /xin/recipe.sh          (ro)
//! <build>/inputs/by-name/<alias>          symlinks; direct mode: absolute
//!                                         store paths, container mode:
//!                                         ../../<oh> under /xin/inputs (ro)
//! <build>/out/{payload,runtime-inputs}/ -> /xin/out                (rw)
//! <build>/work/                         -> /xin/work  (cwd, HOME)  (rw)
//! each input <store>/outputs/<sh>/<oh>  -> /xin/<oh>               (ro)
//! ```
//!
//! Container builds have no network (B3) and see none of the host beyond
//! the toolchain binds (see `container`). The two modes produce the same
//! *output tree* for recipes that only use their declared inputs — which is
//! exactly the class of recipe xin is for.
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
use std::process::Command;

use xin_resolver::events::BuildOutcome;
use xin_resolver::failure::BuildLog;
use xin_resolver::hashes::{InputHash, OutputHash};
use xin_resolver::input::InputName;

use crate::container::{Bwrap, ContainerMode};
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
    mode: &ContainerMode,
) -> io::Result<BuildRun> {
    let ih_short = &input.to_string()[..8];
    let dir = store.new_temp_dir(&format!("build-{ih_short}"))?;
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("payload"))?;
    std::fs::create_dir_all(out.join("runtime-inputs"))?;
    let by_name = dir.join("inputs").join("by-name");
    std::fs::create_dir_all(&by_name)?;
    let containered = matches!(mode, ContainerMode::Bwrap { .. });
    let mut resolved: Vec<(&InputName, OutputHash, PathBuf)> = Vec::new();
    for (alias, oh) in inputs {
        let src = find_input(*oh).ok_or_else(|| {
            io::Error::other(format!(
                "input {oh:?} for alias {alias} is not in any local store"
            ))
        })?;
        // container mode gets the canonical relocatable link form (B4);
        // it resolves against /xin, not against the host store layout
        if containered {
            std::os::unix::fs::symlink(format!("../../{oh}"), by_name.join(alias.as_str()))?;
        } else {
            std::os::unix::fs::symlink(&src, by_name.join(alias.as_str()))?;
        }
        resolved.push((alias, *oh, src));
    }
    let script = dir.join("recipe.sh");
    std::fs::write(&script, recipe)?;

    let mut cmd = match mode {
        ContainerMode::Direct => {
            let mut cmd = Command::new("bash");
            cmd.arg(&script)
                .current_dir(&dir)
                .env("XIN_OUT", &out)
                .env("XIN_PAYLOAD", out.join("payload"))
                .env("XIN_INPUTS", &by_name);
            for (alias, oh, _) in &resolved {
                cmd.env(
                    format!("XIN_INPUT_HASH_{}", alias.as_str().replace('-', "_")),
                    oh.to_string(),
                );
            }
            cmd
        }
        ContainerMode::Bwrap { bwrap } => {
            let work = dir.join("work");
            std::fs::create_dir_all(&work)?;
            let mut bw = Bwrap::new(bwrap, false); // B3: no network
            bw.host_toolchain(true);
            let mut bound = std::collections::BTreeSet::new();
            for (alias, oh, src) in &resolved {
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
            bw.ro_bind(&script, "/xin/recipe.sh");
            bw.bind(&work, "/xin/work");
            bw.chdir("/xin/work");
            bw.setenv("XIN_OUT", "/xin/out");
            bw.setenv("XIN_PAYLOAD", "/xin/out/payload");
            bw.setenv("XIN_INPUTS", "/xin/inputs/by-name");
            bw.setenv("HOME", "/xin/work");
            bw.command(&["bash", "/xin/recipe.sh"])
        }
    };
    let result = cmd.output()?; // no timeout yet: prototype builder
    let log = BuildLog {
        stdout: result.stdout,
        stderr: result.stderr,
        return_code: result.status.code().unwrap_or(-1),
    };

    if !result.status.success() {
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
