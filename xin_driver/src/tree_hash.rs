//! Output hashing over a whole node tree (B12, prototype cut): a canonical,
//! length-prefixed depth-first serialization of names, kinds, exec bits,
//! file contents, and literal symlink targets — no timestamps, no owners,
//! no other permission bits. Entries are visited in byte-order of their
//! names. Disallowed for now: non-UTF-8 names and anything that is not a
//! file, directory, or symlink (sparse/hardlink handling is post-prototype
//! research, B12).

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use xin_resolver::hashes::{HASH_LEN, OutputHash};

fn put(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn walk(h: &mut blake3::Hasher, dir: &Path) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io::Error::other(format!("non-UTF-8 file name in output tree: {entry:?}"))
        })?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let target = target
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF-8 symlink target in output tree"))?;
            put(h, b"l");
            put(h, name.as_bytes());
            put(h, target.as_bytes());
        } else if meta.is_dir() {
            put(h, b"d");
            put(h, name.as_bytes());
            put(h, b"(");
            walk(h, &path)?;
            put(h, b")");
        } else if meta.is_file() {
            let exec = meta.permissions().mode() & 0o111 != 0;
            put(h, if exec { b"x" } else { b"f" });
            put(h, name.as_bytes());
            h.update(&meta.len().to_le_bytes());
            let mut f = fs::File::open(&path)?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
        } else {
            return Err(io::Error::other(format!(
                "unsupported file type in output tree: {path:?} (B12: no sockets/fifos/devices)"
            )));
        }
    }
    Ok(())
}

/// Hash a node tree (the directory that will become outputs/<oh>).
pub fn hash_tree(root: &Path) -> io::Result<OutputHash> {
    let mut h = blake3::Hasher::new();
    put(&mut h, b"xin-tree-v1");
    walk(&mut h, root)?;
    let mut out = [0u8; HASH_LEN];
    out.copy_from_slice(h.finalize().as_bytes());
    Ok(OutputHash(out))
}
