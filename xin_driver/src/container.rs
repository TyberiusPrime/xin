//! Containerized execution via bubblewrap (B3/B4).
//!
//! Builds run under `--unshare-all` (no network — B3) with the virtualized
//! layout below `/xin` from B4: the staged output at `/xin/out`, every
//! input at `/xin/<output-hash>`, aliases at `/xin/inputs/by-name/<name>`
//! (relative symlinks `../../<output-hash>`, so the in-container view is
//! exactly the relocatable representation the store uses). The host is
//! invisible except for the toolchain binds below.
//!
//! Toolchain impurity, deliberate for now: `/nix`, `/run/current-system`,
//! `/usr`, `/bin`, `/lib`, `/lib64` are bound read-only and `PATH` passes
//! through, because a shell and coreutils have to come from *somewhere*
//! until toolchains are ordinary build inputs (B3 TODO). Everything else —
//! home, host tmp, the project tree, the network — is gone.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the config / CLI asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContainerPref {
    /// bwrap when available, direct execution otherwise
    Auto,
    /// bwrap or error
    Bwrap,
    /// direct execution (the pre-container builder)
    None,
}

impl ContainerPref {
    pub fn parse(s: &str) -> Option<ContainerPref> {
        match s {
            "auto" => Some(ContainerPref::Auto),
            "bwrap" => Some(ContainerPref::Bwrap),
            "none" => Some(ContainerPref::None),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ContainerMode {
    Direct,
    Bwrap { bwrap: PathBuf },
}

impl ContainerMode {
    pub fn detect(pref: ContainerPref) -> io::Result<ContainerMode> {
        match pref {
            ContainerPref::None => Ok(ContainerMode::Direct),
            ContainerPref::Bwrap => match find_in_path("bwrap") {
                Some(bwrap) => Ok(ContainerMode::Bwrap { bwrap }),
                None => Err(io::Error::other(
                    "container mode \"bwrap\" requested but no bwrap in PATH",
                )),
            },
            ContainerPref::Auto => Ok(match find_in_path("bwrap") {
                Some(bwrap) => ContainerMode::Bwrap { bwrap },
                None => ContainerMode::Direct,
            }),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ContainerMode::Direct => "none",
            ContainerMode::Bwrap { .. } => "bwrap",
        }
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

    /// The documented toolchain impurity: host shell + tools, read-only.
    /// `with_nix` = false drops `/nix` (and `/run/current-system`, which
    /// only points into it) for containers that bring their own tools.
    pub fn host_toolchain(&mut self, with_nix: bool) {
        if with_nix {
            self.ro_bind_try("/nix", "/nix");
            self.ro_bind_try("/run/current-system", "/run/current-system");
        }
        for d in ["/usr", "/bin", "/lib", "/lib64", "/etc/ssl", "/etc/static"] {
            self.ro_bind_try(d, d);
        }
        if let Ok(path) = std::env::var("PATH") {
            self.setenv("PATH", path);
        }
    }

    pub fn command(&self, argv: &[impl AsRef<std::ffi::OsStr>]) -> Command {
        let mut cmd = Command::new(&self.bwrap);
        cmd.args(&self.args);
        cmd.arg("--");
        cmd.args(argv.iter().map(|a| a.as_ref()));
        cmd
    }
}
