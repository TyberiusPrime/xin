//! Garbage collection over local stores.
//!
//! The filesystem is the database (B5): the live set is derived, never
//! cached. Roots are (a) indirect gc-roots — symlinks to user-facing
//! results links, dropped when that link is gone — and (b) gc-protect
//! leases whose holding process is still alive (A6; a dead pid means a
//! crashed run, and its protection dies with it). The live set is the
//! closure of the roots over declared runtime refs, computed across *all*
//! given stores, then each store is swept against it — a root in one store
//! keeps its runtime closure alive even where parts live in another store.
//!
//! Also swept, per B5's "failed builds stick around ... no longer than the
//! next gc": temp/ build dirs of dead processes and meta/ entries whose
//! input mapping no longer exists.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use xin_resolver::hashes::OutputHash;

use crate::local_store::LocalStore;

#[derive(Clone, Debug, Default)]
pub struct GcReport {
    /// outputs still reachable from this store's roots+leases (whole-run
    /// live set; identical across stores of one run)
    pub live_outputs: usize,
    pub deleted_outputs: Vec<OutputHash>,
    pub freed_bytes: u64,
    pub pruned_inputs: usize,
    pub pruned_meta: usize,
    pub cleaned_temp: usize,
    /// gc-roots entries whose results link vanished
    pub dropped_roots: usize,
    /// gc-protect leases whose owning process is dead
    pub dropped_leases: usize,
    pub kept_roots: usize,
    pub kept_leases: usize,
}

/// Is the process holding this lease/temp dir still alive? Linux: /proc.
/// Where /proc is unavailable we answer "alive" — conservative, never
/// deletes something a live process relies on.
fn pid_alive(pid: u32) -> bool {
    if !Path::new("/proc").is_dir() {
        return true;
    }
    Path::new("/proc").join(pid.to_string()).exists()
}

/// `<pid>-<rest>` → pid (lease dirs); `<tag>-<pid>-<n>` → pid (temp dirs,
/// parsed from the right because the tag may contain dashes).
fn leading_pid(name: &str) -> Option<u32> {
    name.split('-').next()?.parse().ok()
}
fn embedded_pid(name: &str) -> Option<u32> {
    let mut it = name.rsplit('-');
    it.next()?; // the counter
    it.next()?.parse().ok()
}

fn oh_from_link_target(target: &Path) -> Option<OutputHash> {
    target
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(OutputHash::parse)
}

/// Recursive delete that shrugs off read-only bits builds may have left:
/// on failure, chmod everything user-writeable and retry once.
fn force_remove_dir_all(dir: &Path) -> io::Result<()> {
    if fs::remove_dir_all(dir).is_ok() {
        return Ok(());
    }
    fn make_writeable(p: &Path) {
        if let Ok(meta) = p.symlink_metadata() {
            let mut perm = meta.permissions();
            use std::os::unix::fs::PermissionsExt;
            perm.set_mode(perm.mode() | if meta.is_dir() { 0o700 } else { 0o600 });
            let _ = fs::set_permissions(p, perm);
        }
        if p.is_dir()
            && !p.symlink_metadata().map(|m| m.is_symlink()).unwrap_or(true)
            && let Ok(rd) = fs::read_dir(p)
        {
            for e in rd.flatten() {
                make_writeable(&e.path());
            }
        }
    }
    make_writeable(dir);
    fs::remove_dir_all(dir)
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(meta) = e.path().symlink_metadata() {
                if meta.is_dir() {
                    total += dir_size(&e.path());
                } else {
                    total += meta.len();
                }
            }
        }
    }
    total
}

/// Every `<shard>/<entry>` pair under `base` whose entry name parses as a
/// hash display form; unparseable strays are left alone.
fn sharded_entries(base: &Path) -> io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    if !base.is_dir() {
        return Ok(out);
    }
    for shard in fs::read_dir(base)? {
        let shard = shard?.path();
        if !shard.is_dir() {
            continue;
        }
        for e in fs::read_dir(&shard)? {
            out.push(e?.path());
        }
    }
    out.sort();
    Ok(out)
}

struct StorePrep {
    roots: BTreeSet<OutputHash>,
    protected: BTreeSet<OutputHash>,
    stale_roots: Vec<std::path::PathBuf>,
    stale_leases: Vec<std::path::PathBuf>,
    kept_roots: usize,
    kept_leases: usize,
}

/// Phase 1 (read-only): classify this store's roots and leases.
fn prep_store(store: &LocalStore) -> io::Result<StorePrep> {
    let mut prep = StorePrep {
        roots: BTreeSet::new(),
        protected: BTreeSet::new(),
        stale_roots: Vec::new(),
        stale_leases: Vec::new(),
        kept_roots: 0,
        kept_leases: 0,
    };
    for (entry, target) in store.roots()? {
        // the target is the user-facing results symlink; it counts as a
        // root only while it still exists and points at an output dir
        match fs::read_link(&target).ok().and_then(|t| {
            let abs = if t.is_relative() {
                target.parent().map(|p| p.join(&t))?
            } else {
                t
            };
            oh_from_link_target(&abs)
        }) {
            Some(oh) => {
                prep.roots.insert(oh);
                prep.kept_roots += 1;
            }
            None => prep.stale_roots.push(entry),
        }
    }
    for lease in fs::read_dir(store.root.join("gc-protect"))? {
        let lease = lease?.path();
        let name = lease.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let alive = leading_pid(name).map(pid_alive).unwrap_or(true);
        if !alive {
            prep.stale_leases.push(lease);
            continue;
        }
        prep.kept_leases += 1;
        for e in fs::read_dir(&lease)? {
            if let Ok(t) = fs::read_link(e?.path())
                && let Some(oh) = oh_from_link_target(&t)
            {
                prep.protected.insert(oh);
            }
        }
    }
    Ok(prep)
}

/// Close `live` over declared runtime refs, looking outputs up across all
/// stores. Refs of absent outputs contribute nothing (they are what gc is
/// allowed to have removed earlier / never had).
fn close_over_runtime_refs(stores: &[&LocalStore], live: &mut BTreeSet<OutputHash>) {
    let mut queue: Vec<OutputHash> = live.iter().copied().collect();
    while let Some(oh) = queue.pop() {
        for store in stores {
            let ri = store.output_dir(oh).join("runtime-inputs");
            let Ok(rd) = fs::read_dir(&ri) else { continue };
            for e in rd.flatten() {
                if let Ok(t) = fs::read_link(e.path())
                    && let Some(dep) = t
                        .to_str()
                        .and_then(|t| t.strip_prefix("../../"))
                        .and_then(OutputHash::parse)
                    && live.insert(dep)
                {
                    queue.push(dep);
                }
            }
            break; // first store that has the output wins; refs are identical (CAS)
        }
    }
}

/// The runtime closure of one output as recorded on disk: the output plus
/// everything reachable over declared runtime refs across the given stores.
/// This is the mount set for an interactive container (`xin shell`).
pub fn runtime_closure(stores: &[&LocalStore], root: OutputHash) -> BTreeSet<OutputHash> {
    let mut live = BTreeSet::from([root]);
    close_over_runtime_refs(stores, &mut live);
    live
}

/// Phase 2: sweep one store against the global live set.
fn sweep_store(
    store: &LocalStore,
    prep: &StorePrep,
    live: &BTreeSet<OutputHash>,
    dry_run: bool,
) -> io::Result<GcReport> {
    let mut report = GcReport {
        live_outputs: live.len(),
        kept_roots: prep.kept_roots,
        kept_leases: prep.kept_leases,
        dropped_roots: prep.stale_roots.len(),
        dropped_leases: prep.stale_leases.len(),
        ..GcReport::default()
    };
    if !dry_run {
        for r in &prep.stale_roots {
            fs::remove_file(r)?;
        }
        for l in &prep.stale_leases {
            force_remove_dir_all(l)?;
        }
    }

    for dir in sharded_entries(&store.root.join("outputs"))? {
        let Some(oh) = oh_from_link_target(&dir) else {
            continue;
        };
        if live.contains(&oh) {
            continue;
        }
        report.freed_bytes += dir_size(&dir);
        report.deleted_outputs.push(oh);
        if !dry_run {
            force_remove_dir_all(&dir)?;
        }
    }

    for link in sharded_entries(&store.root.join("inputs"))? {
        let keep = fs::read_link(&link)
            .ok()
            .and_then(|t| oh_from_link_target(&t))
            .is_some_and(|oh| live.contains(&oh));
        if !keep {
            report.pruned_inputs += 1;
            if !dry_run {
                fs::remove_file(&link)?;
            }
        }
    }

    // meta follows its mapping: no inputs/<ih> link (never committed, or
    // pruned just now) => the log's build is gone too
    for meta in sharded_entries(&store.root.join("meta").join("by_input"))? {
        let ih = meta.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let link = store
            .root
            .join("inputs")
            .join(&ih[..2.min(ih.len())])
            .join(ih);
        let mapping_kept = link.symlink_metadata().is_ok()
            && fs::read_link(&link)
                .ok()
                .and_then(|t| oh_from_link_target(&t))
                .is_some_and(|oh| live.contains(&oh));
        if !mapping_kept {
            report.pruned_meta += 1;
            if !dry_run {
                force_remove_dir_all(&meta)?;
            }
        }
    }

    for tmp in fs::read_dir(store.root.join("temp"))? {
        let tmp = tmp?.path();
        let name = tmp.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if embedded_pid(name).map(pid_alive).unwrap_or(false) {
            continue; // a live process is still building in there
        }
        report.cleaned_temp += 1;
        if !dry_run {
            force_remove_dir_all(&tmp)?;
        }
    }

    Ok(report)
}

/// Run gc across a set of local stores (typically: every writeable local
/// store in the config). Returns one report per store, in input order.
pub fn run_gc(stores: &[&LocalStore], dry_run: bool) -> io::Result<Vec<GcReport>> {
    let mut preps = Vec::new();
    let mut live = BTreeSet::new();
    for store in stores {
        let prep = prep_store(store)?;
        live.extend(prep.roots.iter().copied());
        live.extend(prep.protected.iter().copied());
        preps.push(prep);
    }
    close_over_runtime_refs(stores, &mut live);

    let mut reports = Vec::new();
    for (store, prep) in stores.iter().zip(&preps) {
        reports.push(sweep_store(store, prep, &live, dry_run)?);
    }
    Ok(reports)
}

/// Everything an inspection command wants to say about one store, derived
/// on the spot (B5: indexes are disposable).
#[derive(Clone, Debug, Default)]
pub struct StoreInventory {
    pub outputs: Vec<(OutputHash, u64)>,
    pub mappings: usize,
    pub roots: Vec<(String, String)>,
    pub leases: usize,
    pub temp_dirs: usize,
}

pub fn inventory(store: &LocalStore) -> io::Result<StoreInventory> {
    let mut inv = StoreInventory::default();
    for dir in sharded_entries(&store.root.join("outputs"))? {
        if let Some(oh) = oh_from_link_target(&dir) {
            inv.outputs.push((oh, dir_size(&dir)));
        }
    }
    inv.mappings = sharded_entries(&store.root.join("inputs"))?.len();
    for (entry, target) in store.roots()? {
        inv.roots.push((
            entry
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned(),
            target.display().to_string(),
        ));
    }
    inv.leases = fs::read_dir(store.root.join("gc-protect"))?.count();
    inv.temp_dirs = fs::read_dir(store.root.join("temp"))?.count();
    Ok(inv)
}
