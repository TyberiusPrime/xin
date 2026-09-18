//! The mandatory build sandbox: bubblewrap + a bootstrap busybox (B3/B4).
//!
//! There is no unsandboxed build path. Every recipe runs under
//! `--unshare-all` (no network — B3) in a container whose *entire* world is
//! the B4 layout below `/xin`: the staged output at `/xin/out`, every input
//! at `/xin/<output-hash>`, aliases at `/xin/inputs/by-name/<name>`
//! (relative `../../<output-hash>` symlinks — the store's relocatable
//! representation), and `/xin/bootstrap`. No `/nix`, no `/usr`, no host
//! `PATH`: toolchains beyond the bootstrap are ordinary build inputs, and
//! recipes reference them through `$XIN_INPUTS` (e.g.
//! `exec "$XIN_INPUTS/python/payload/bin/python3" ...`).
//!
//! The bootstrap is a single *statically linked* busybox (the dev flake
//! provides `pkgsStatic.busybox`) staged as `/xin/bootstrap/busybox` plus
//! one symlink per applet; `PATH=/xin/bootstrap` and recipes are executed
//! by `/xin/bootstrap/sh`. It is the one impurity the input-hash does not
//! yet cover — pinning it as a fixed-output input is the designated fix
//! (design.md B3 TODO); keeping it minimal and static keeps the surface
//! small in the meantime.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub bwrap: PathBuf,
    /// the statically linked bootstrap busybox
    pub busybox: PathBuf,
    /// its applet names (from `busybox --list`), the bootstrap symlink farm
    applets: Vec<String>,
}

impl Sandbox {
    /// Builds are sandbox-mandatory, so failing here fails loudly and
    /// early. The bootstrap comes from (in order): the explicit override
    /// (config `bootstrap = ...`), `$XIN_BOOTSTRAP`, or `busybox` on PATH.
    pub fn detect(bootstrap: Option<&Path>) -> io::Result<Sandbox> {
        let bwrap = find_in_path("bwrap").ok_or_else(|| {
            io::Error::other(
                "builds are sandboxed, no exceptions: bwrap is required but not in PATH",
            )
        })?;
        let busybox = bootstrap
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("XIN_BOOTSTRAP").map(PathBuf::from))
            .or_else(|| find_in_path("busybox"))
            .ok_or_else(|| {
                io::Error::other(
                    "no bootstrap busybox: set `bootstrap` in xin.config.toml, $XIN_BOOTSTRAP, \
                     or put a static busybox in PATH (the dev flake ships pkgsStatic.busybox)",
                )
            })?;
        let list = Command::new(&busybox).arg("--list").output()?;
        if !list.status.success() {
            return Err(io::Error::other(format!(
                "{} does not answer `--list`; the bootstrap must be a full static busybox \
                 (nixpkgs' minimal bootstrap busybox is ash-only and will not do)",
                busybox.display()
            )));
        }
        let applets: Vec<String> = String::from_utf8_lossy(&list.stdout)
            .lines()
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != "busybox")
            .map(str::to_owned)
            .collect();
        if !applets.iter().any(|a| a == "sh") {
            return Err(io::Error::other(format!(
                "{} has no `sh` applet; recipes cannot run",
                busybox.display()
            )));
        }
        Ok(Sandbox {
            bwrap,
            busybox,
            applets,
        })
    }

    /// Materialize the bootstrap into `<dir>/bootstrap`: a *copy* of the
    /// busybox binary (the container cannot see the host path a symlink
    /// would point at) plus one relative symlink per applet.
    pub fn stage_bootstrap(&self, dir: &Path) -> io::Result<PathBuf> {
        let boot = dir.join("bootstrap");
        std::fs::create_dir_all(&boot)?;
        std::fs::copy(&self.busybox, boot.join("busybox"))?;
        for applet in &self.applets {
            std::os::unix::fs::symlink("busybox", boot.join(applet))?;
        }
        Ok(boot)
    }
}

pub fn find_in_path(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|c| c.is_file())
    })
}

/// A bwrap invocation under construction. Every container gets a private
/// /dev, /proc and /tmp, a cleared environment, hostname `xin`, and dies
/// with its parent; everything else is opt-in.
pub struct Bwrap {
    bwrap: PathBuf,
    args: Vec<OsString>,
}

impl Bwrap {
    pub fn new(bwrap: &Path, network: bool) -> Bwrap {
        let mut b = Bwrap {
            bwrap: bwrap.to_path_buf(),
            args: Vec::new(),
        };
        if network {
            // interactive use wants ports and downloads; still a private
            // pid/ipc/uts world
            b.push(["--unshare-pid", "--unshare-ipc", "--unshare-uts"]);
        } else {
            b.push(["--unshare-all"]);
        }
        b.push(["--hostname", "xin", "--die-with-parent"]);
        b.push(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]);
        b.push(["--clearenv"]);
        b.setenv("TMPDIR", "/tmp");
        b
    }

    fn push<const N: usize>(&mut self, args: [&str; N]) {
        self.args.extend(args.iter().map(OsString::from));
    }

    fn push_path(&mut self, flag: &str, a: impl AsRef<Path>, b: impl AsRef<Path>) {
        self.args.push(flag.into());
        self.args.push(a.as_ref().into());
        self.args.push(b.as_ref().into());
    }

    pub fn ro_bind(&mut self, src: impl AsRef<Path>, dst: impl AsRef<Path>) {
        self.push_path("--ro-bind", src, dst);
    }

    pub fn ro_bind_try(&mut self, src: impl AsRef<Path>, dst: impl AsRef<Path>) {
        self.push_path("--ro-bind-try", src, dst);
    }

    pub fn bind(&mut self, src: impl AsRef<Path>, dst: impl AsRef<Path>) {
        self.push_path("--bind", src, dst);
    }

    pub fn setenv(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.args.push("--setenv".into());
        self.args.push(key.into());
        self.args.push(value.as_ref().into());
    }

    pub fn chdir(&mut self, dir: impl AsRef<Path>) {
        self.args.push("--chdir".into());
        self.args.push(dir.as_ref().into());
    }

    pub fn command(&self, argv: &[impl AsRef<std::ffi::OsStr>]) -> Command {
        let mut cmd = Command::new(&self.bwrap);
        cmd.args(&self.args);
        cmd.arg("--");
        cmd.args(argv.iter().map(|a| a.as_ref()));
        cmd
    }
}
