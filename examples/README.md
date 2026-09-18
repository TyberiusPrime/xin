# xin by example

xin is a content-addressed build system for scientific pipelines: you
describe a DAG of build steps in [Nickel](https://nickel-lang.org/), xin
hashes every step's inputs, and anything already built — here, on a
shared store, on a remote cache — is never built again. Outputs are
immutable, named by the hash of their bytes, and garbage-collected only
when nothing you kept points at them anymore.

## Prerequisites

- the `xin` binary (`cargo build -p xin`)
- `bwrap` (bubblewrap) — **builds only run sandboxed, there is no
  opt-out**
- a *statically linked* full busybox as the sandbox bootstrap

The dev flake provides the last two (`nix develop`). Outside of it, point
xin at a bootstrap explicitly, either per project in `xin.config.toml`:

```toml
bootstrap = "/path/to/static/busybox"
```

or via `$XIN_BOOTSTRAP`, or just have `busybox` in `PATH`.

## The shape of every example

```
some-example/
  xin.config.toml     where the store lives, policy knobs
  *.xin.ncl           the DAG definition (Nickel)
  store/              created on first build   (never commit this)
  results/            symlinks to your targets (never commit this)
```

Run them from inside the example directory:

```console
$ cd 01-hello
$ xin build hello        # 'hello' means hello.xin.ncl
```

`xin build` with no argument works too when the directory holds exactly
one `*.xin.ncl`.

## What a recipe is allowed to see

Recipes run in a container whose entire world is:

| path                       | what                                          |
| -------------------------- | --------------------------------------------- |
| `/xin/out/payload/`        | write your results here (`$XIN_PAYLOAD`)      |
| `/xin/out/runtime-inputs/` | declare runtime dependencies here             |
| `/xin/<output-hash>/`      | each declared input, read-only                |
| `/xin/inputs/by-name/<alias>/` | the same inputs, by alias (`$XIN_INPUTS`) |
| `/xin/work/`               | scratch space, cwd and `$HOME`                |
| `/xin/bootstrap/`          | busybox: `sh`, `awk`, `tr`, ... (`$PATH`)     |

No network (fetch nodes excepted), no `/nix`, no `/usr`, no host
anything. The shell is busybox **ash**, not bash. A real toolchain is an
ordinary input: `exec "$XIN_INPUTS/python/payload/bin/python3" ...`.

Per input alias, `$XIN_INPUT_HASH_<alias>` holds the input's
output-hash, so declaring a runtime dependency is one canonical line:

```sh
ln -s "../../$XIN_INPUT_HASH_data" "$XIN_OUT/runtime-inputs/data"
```

## The examples

| example                          | shows                                                        |
| -------------------------------- | ------------------------------------------------------------ |
| [01-hello](01-hello/)            | one node: build, results/, rebuilds are free                 |
| [02-pipeline](02-pipeline/)      | a chain: aliases, runtime deps, early cutoff, `xin shell`    |
| [03-fetch-tofu](03-fetch-tofu/)  | a fetched input, pinned by trust-on-first-use                |
| [04-fanout](04-fanout/)          | Nickel as a generator: computed workers, computed recipes    |

## Commands you will keep using

```console
$ xin build [FILE]         # realize the targets
$ xin status [FILE]        # what exists / what would build — builds nothing
$ xin log NODE [FILE]      # stdout/stderr/exit of a node's build
$ xin dag [FILE]           # the pruned DAG, topologically ordered
$ xin eval [FILE]          # the TOML the Nickel file evaluates to
$ xin store ls             # what's in the store
$ xin gc [--dry-run]       # sweep everything results/ no longer points at
$ xin shell NODE [FILE]    # interactive container with NODE mounted
```

Every command takes `--format json` for the full, scripting-friendly
report; the human output is a condensed view of the same data.
