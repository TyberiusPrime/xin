# 02 — a pipeline

Four nodes: `raw → clean → stats → report`, with `report` also keeping a
**runtime dependency** on `clean`.

```console
$ xin dag pipeline        # look before you build
$ xin build pipeline
✓ raw     realized ...
✓ clean   realized ...
✓ stats   realized ...
✓ report  realized ...
ok: 4 builds run
$ cat results/report/payload/report.txt
n=3 mean=1166.0
data: see runtime-inputs/clean/payload/samples.csv
```

## Build inputs vs runtime inputs

`inputs.<alias> = "<node>"` wires the DAG: the upstream's files appear at
`$XIN_INPUTS/<alias>/...` during the build, and its output-hash is part
of this node's input-hash.

That relationship *ends* with the build — unless the recipe declares it
should not:

```sh
ln -s "../../$XIN_INPUT_HASH_clean" "$XIN_OUT/runtime-inputs/clean"
```

That link (relative, content-addressed, relocatable) is a promise: this
output needs `clean` present wherever it goes. `xin gc` keeps the whole
runtime closure of anything in `results/`; substitution from remote
stores downloads closures along these links.

The link's *content* is the dependency's output-hash, nothing more:

```console
$ readlink results/report/runtime-inputs/clean
../../77aj6ldd...B
```

Note it does not resolve against the host store (outputs are sharded
into subdirectories on disk) — deliberately. The link is part of the
hashed output and must mean the same thing in every store on every
machine, so it resolves against the *container* view, where every
output sits flat under `/xin/`:

```console
$ xin shell report -- sh -c 'cat "$XIN_NODE/runtime-inputs/clean/payload/samples.csv"'
```

## Early cutoff: the flagship trick

Add a comment line to the **raw** recipe and rebuild:

```console
$ xin build pipeline
ok: 1 build run
```

One build, not four. `raw`'s input-hash changed, so it re-ran — but it
produced byte-identical output, so the same output-hash came out, so
`clean`'s input-hash did not change, and the rest of the chain was
served from the store. Compare Merkle-style systems (nix), which rebuild
the entire downstream cone.

## Step inside

```console
$ xin shell report                              # interactive
$ xin shell report -- sh -c 'ls "$XIN_NODE"'    # one-shot command
```

`xin shell` realizes the node if needed, then opens a container with the
node and its full runtime closure mounted at `/xin/<output-hash>` —
exactly the layout its runtime-input links resolve against. Your current
directory is read-write at `/xin/work`; the host `/nix` is mounted for
interactive comfort (jupyter, editors) unless you pass `--no-nix`.
`$XIN_NODE` points at the node's directory.

## Clean up — and what "alive" means

```console
$ xin gc --dry-run
main (...): 2 live, would be: 2 output(s) removed ...
```

Only `report` and `clean` are live: `results/report` roots the target,
and the runtime-input link pulls in `clean`. `raw` and `stats` were
*build-time* inputs — nothing needs them anymore, and gc reclaims them.
(They'd be rebuilt — or refetched from a cache — if a recipe changes and
needs them again; the input→output mappings that make that cheap are
kept as long as their outputs are.)

```console
$ rm results/report
$ xin gc                  # now everything goes
```

## Failure policy

Break a recipe (e.g. make `clean` `exit 3`) and rebuild: with the
config's `keep-going`, independent nodes still build and `report` gets
blamed via its failed upstream (`xin log clean` shows stderr). Pass
`--fail-fast` to stop scheduling new work at the first failure instead.
