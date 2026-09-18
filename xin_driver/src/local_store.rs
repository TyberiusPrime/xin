//! A local store on the filesystem, laid out per design.md B5:
//!
//! ```text
//! root/outputs/<sh>/<output-hash>/{payload/, runtime-inputs/}   (A1 node layout)
//! root/inputs/<sh>/<input-hash>  -> ../../outputs/<sh>/<output-hash>
//! root/temp/<build dirs>                                        (same fs => atomic rename)
//! root/gc-protect/<pid>-<lease>/<n> -> dangling-ok symlinks      (A6)
//! root/gc-roots/<h>              -> /abs/path/of/results-symlink  (indirect roots)
//! root/meta/by_input/<sh>/<input-hash>/{stdout,stderr,exit}
//! ```
//!
//! `<sh>` is the first two characters of the hash's display form (B5
//! sharding). runtime-inputs entries carry the *canonical* link content
//! `../../<output-hash>` from A1 — the output-hash covers that literal
//! string, and resolving it on a sharded disk layout is a store/view
//! concern, not the resolver's (plan-feedback "A4 cross-store symlinks").
//! No fsyncs yet; the commit protocol is right, durability is not (B5 TODO).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::{fs, process};

use xin_resolver::events::{CommitResult, LeaseId, Presence};
use xin_resolver::failure::BuildLog;
use xin_resolver::hashes::{InputHash, OutputHash};

pub struct LocalStore {
    pub root: PathBuf,
    next_lease: u64,
    next_temp: u64,
}

fn sharded(base: PathBuf, name: &str) -> PathBuf {
    base.join(&name[..2]).join(name)
}

impl LocalStore {
    /// Open (creating layout directories as needed — idempotent).
    pub fn open(root: impl Into<PathBuf>) -> io::Result<LocalStore> {
        let root = root.into();
        for d in [
            "outputs",
            "inputs",
            "temp",
            "gc-protect",
            "gc-roots",
            "meta",
        ] {
            fs::create_dir_all(root.join(d))?;
        }
        Ok(LocalStore {
            root,
            next_lease: 0,
            next_temp: 0,
        })
    }

    pub fn output_dir(&self, oh: OutputHash) -> PathBuf {
        sharded(self.root.join("outputs"), &oh.to_string())
    }

    fn input_link(&self, ih: InputHash) -> PathBuf {
        sharded(self.root.join("inputs"), &ih.to_string())
    }

    pub fn lookup_mapping(&self, ih: InputHash) -> io::Result<Option<OutputHash>> {
        match fs::read_link(self.input_link(ih)) {
            Ok(target) => {
                let name = target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(OutputHash::parse)
                    .ok_or_else(|| {
                        io::Error::other(format!("corrupt input link for {ih}: {target:?}"))
                    })?;
                Ok(Some(name))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn presence(&self, oh: OutputHash) -> io::Result<Presence> {
        let dir = self.output_dir(oh);
        if !dir.is_dir() {
            return Ok(Presence::Missing);
        }
        Ok(Presence::Present {
            runtime_refs: read_runtime_refs(&dir)?,
        })
    }

    /// A6: protection must hold regardless of current existence, so the
    /// gc-protect entries may dangle. The directory name carries our pid so
    /// concurrent processes cannot collide and `gc` can recognize leases
    /// whose holder died.
    fn lease_dir(&self, lease: LeaseId) -> PathBuf {
        self.root
            .join("gc-protect")
            .join(format!("{}-{}", process::id(), lease.0))
    }

    pub fn acquire_lease(&mut self, protect: &BTreeSet<OutputHash>) -> io::Result<LeaseId> {
        self.next_lease += 1;
        let dir = self.lease_dir(LeaseId(self.next_lease));
        fs::create_dir_all(&dir)?;
        for (i, oh) in protect.iter().enumerate() {
            let name = oh.to_string();
            let target = format!("../../outputs/{}/{}", &name[..2], name);
            std::os::unix::fs::symlink(target, dir.join(i.to_string()))?;
        }
        Ok(LeaseId(self.next_lease))
    }

    pub fn release_lease(&self, lease: LeaseId) -> io::Result<()> {
        fs::remove_dir_all(self.lease_dir(lease))
    }

    pub fn held_leases(&self) -> io::Result<usize> {
        Ok(fs::read_dir(self.root.join("gc-protect"))?.count())
    }

    /// A fresh directory under temp/ — same filesystem as outputs/, so the
    /// eventual commit is one rename (B5, B11).
    pub fn new_temp_dir(&mut self, tag: &str) -> io::Result<PathBuf> {
        self.next_temp += 1;
        let dir =
            self.root
                .join("temp")
                .join(format!("{tag}-{}-{}", process::id(), self.next_temp));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Commit point 1: rename(temp → outputs/<oh>). A destination that
    /// already exists means a concurrent build won — same bytes (CAS), so
    /// drop ours and report `already_existed`.
    pub fn commit_output(&self, oh: OutputHash, staged: &Path) -> io::Result<bool> {
        let dest = self.output_dir(oh);
        fs::create_dir_all(dest.parent().unwrap())?;
        match fs::rename(staged, &dest) {
            Ok(()) => Ok(false),
            Err(_) if dest.is_dir() => {
                fs::remove_dir_all(staged)?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    /// Commit point 2: symlink(inputs/<ih> → outputs/<oh>). EEXIST with a
    /// different target is the nondeterminism detector (B5) and comes back
    /// as `Conflict`, never as an error string.
    pub fn commit_mapping(&self, ih: InputHash, oh: OutputHash) -> io::Result<CommitResult> {
        let link = self.input_link(ih);
        fs::create_dir_all(link.parent().unwrap())?;
        let name = oh.to_string();
        let target = format!("../../outputs/{}/{}", &name[..2], name);
        match std::os::unix::fs::symlink(target, &link) {
            Ok(()) => Ok(CommitResult::Committed),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let existing = self.lookup_mapping(ih)?.ok_or_else(|| {
                    io::Error::other("mapping link vanished between EEXIST and readback")
                })?;
                if existing == oh {
                    Ok(CommitResult::Committed)
                } else {
                    Ok(CommitResult::Conflict { existing })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Build logs land in meta/, which is not part of the output (B5);
    /// losing them is harmless, so callers treat this fire-and-forget.
    pub fn write_build_meta(&self, ih: InputHash, log: &BuildLog) -> io::Result<()> {
        let dir = sharded(self.root.join("meta").join("by_input"), &ih.to_string());
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("stdout"), &log.stdout)?;
        fs::write(dir.join("stderr"), &log.stderr)?;
        fs::write(dir.join("exit"), format!("{}\n", log.return_code))?;
        Ok(())
    }

    pub fn read_build_meta(&self, ih: InputHash) -> io::Result<Option<BuildLog>> {
        let dir = sharded(self.root.join("meta").join("by_input"), &ih.to_string());
        if !dir.is_dir() {
            return Ok(None);
        }
        let return_code = fs::read_to_string(dir.join("exit"))?
            .trim()
            .parse::<i32>()
            .map_err(io::Error::other)?;
        Ok(Some(BuildLog {
            stdout: fs::read(dir.join("stdout"))?,
            stderr: fs::read(dir.join("stderr"))?,
            return_code,
        }))
    }

    /// Register an indirect gc root (nix-style): gc-roots/<h> points at a
    /// user-facing symlink (typically results/<name>), which in turn points
    /// at an output. Deleting the results link releases the root — `gc`
    /// drops entries whose target is gone.
    pub fn register_root(&self, link: &Path) -> io::Result<()> {
        let name = blake3::hash(link.as_os_str().as_encoded_bytes())
            .to_hex()
            .to_string();
        let entry = self.root.join("gc-roots").join(&name[..32]);
        match fs::remove_file(&entry) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::os::unix::fs::symlink(link, entry)
    }

    /// All registered indirect roots: (entry path, target path).
    pub fn roots(&self) -> io::Result<Vec<(PathBuf, PathBuf)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("gc-roots"))? {
            let path = entry?.path();
            out.push((path.clone(), fs::read_link(&path)?));
        }
        out.sort();
        Ok(out)
    }
}

/// Read the declared runtime refs of a stored (or just-built) node dir:
/// runtime-inputs/* symlinks whose literal target is `../../<output-hash>`
/// (A1). Anything else in there is a layout violation (B1 lint).
pub fn read_runtime_refs(node_dir: &Path) -> io::Result<BTreeSet<OutputHash>> {
    let ri = node_dir.join("runtime-inputs");
    let mut refs = BTreeSet::new();
    if !ri.is_dir() {
        return Err(io::Error::other(format!(
            "node dir {node_dir:?} has no runtime-inputs/"
        )));
    }
    for entry in fs::read_dir(&ri)? {
        let entry = entry?;
        let target = fs::read_link(entry.path()).map_err(|_| {
            io::Error::other(format!(
                "runtime-inputs entry {:?} is not a symlink",
                entry.path()
            ))
        })?;
        let t = target.to_str().unwrap_or_default();
        let oh = t
            .strip_prefix("../../")
            .and_then(OutputHash::parse)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "runtime-inputs entry {:?} must point at ../../<output-hash>, found {t:?}",
                    entry.path()
                ))
            })?;
        refs.insert(oh);
    }
    Ok(refs)
}
