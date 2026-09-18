//! The Process builder: run the recipe as a shell script in a temp dir laid
//! out like B4's container view, minus the container (that is part c of the
//! design and out of scope here — no sandbox, no /xin virtualization yet):
//!
//! ```text
//! <build>/recipe.sh
//! <build>/inputs/by-name/<alias> -> <store>/outputs/<sh>/<oh>   (absolute)
//! <build>/out/payload/                                          (script writes here)
//! <build>/out/runtime-inputs/                                   (script declares refs here)
//! ```
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
) -> io::Result<BuildRun> {
    let ih_short = &input.to_string()[..8];
    let dir = store.new_temp_dir(&format!("build-{ih_short}"))?;
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("payload"))?;
    std::fs::create_dir_all(out.join("runtime-inputs"))?;
    let by_name = dir.join("inputs").join("by-name");
    std::fs::create_dir_all(&by_name)?;
    for (alias, oh) in inputs {
        let src = find_input(*oh).ok_or_else(|| {
            io::Error::other(format!(
                "input {oh:?} for alias {alias} is not in any local store"
            ))
        })?;
        std::os::unix::fs::symlink(src, by_name.join(alias.as_str()))?;
    }
    let script = dir.join("recipe.sh");
    std::fs::write(&script, recipe)?;

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .current_dir(&dir)
        .env("XIN_OUT", &out)
        .env("XIN_PAYLOAD", out.join("payload"))
        .env("XIN_INPUTS", &by_name);
    for (alias, oh) in inputs {
        cmd.env(
            format!("XIN_INPUT_HASH_{}", alias.as_str().replace('-', "_")),
            oh.to_string(),
        );
    }
    let result = cmd.output()?; // no sandbox, no timeout: prototype builder
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
