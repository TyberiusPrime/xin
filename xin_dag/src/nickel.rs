//! Nickel evaluation, embedded via `nickel-lang-core` — no external binary.
//! A user file `import`s the shipped prelude (`xin.ncl`), which we
//! materialize into a content-addressed directory under the system temp dir
//! and put on the import path. Evaluation exports the TOML intermediary
//! that `schema` parses.

use std::io;
use std::path::{Path, PathBuf};
use std::{env, fs};

use nickel_lang_core::error::NullReporter;
use nickel_lang_core::error::report::{ColorOpt, report_as_str};
use nickel_lang_core::eval::cache::CacheImpl;
use nickel_lang_core::program::Program;
use nickel_lang_core::serialize::{ExportFormat, to_string, validate};
use xin_resolver::hashes::hash_bytes;

use crate::DagError;

pub const PRELUDE: &'static str = include_str!("../assets/xin.ncl");
pub const PRELUDE_FILE: &str = "xin.ncl";


/// Materialize the prelude so `import "xin.ncl"` resolves. The directory is
/// keyed by the prelude's own hash, so concurrent versions never clash and
/// re-writing is idempotent.
pub fn prelude_dir() -> io::Result<PathBuf> {
    let tag = hash_bytes(PRELUDE.as_bytes());
    let mut hex = String::with_capacity(16);
    for b in &tag[..8] {
        hex.push_str(&format!("{b:02x}"));
    }
    let dir = env::temp_dir().join(format!("xin-nickel-prelude-{hex}"));
    fs::create_dir_all(&dir)?;
    let file = dir.join(PRELUDE_FILE);
    if !file.exists() {
        fs::write(&file, PRELUDE)?;
    }
    Ok(dir)
}

/// Evaluate a `.ncl` DAG definition to the TOML intermediary.
pub fn eval_to_toml(path: &Path, extra_import_paths: &[PathBuf]) -> Result<String, DagError> {
    let mut program: Program<CacheImpl> =
        Program::new_from_file(path, io::sink(), NullReporter {}).map_err(DagError::Io)?;
    let mut imports = vec![prelude_dir().map_err(DagError::Io)?];
    imports.extend(extra_import_paths.iter().cloned());
    program.add_import_paths(imports.into_iter());

    let value = program
        .eval_full_for_export()
        .map_err(|e| DagError::Nickel(report_as_str(&mut program.files(), e, ColorOpt::Never)))?;
    validate(ExportFormat::Toml, &value)
        .map_err(|e| DagError::Nickel(format!("not exportable as TOML: {e:?}")))?;
    to_string(ExportFormat::Toml, &value)
        .map_err(|e| DagError::Nickel(format!("TOML serialization failed: {e:?}")))
}
