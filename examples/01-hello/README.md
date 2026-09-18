# 01 — hello

One node that writes one file. The smallest complete xin project:

- `hello.xin.ncl` — the DAG: a single node marked `target = true`
- `xin.config.toml` — one local store at `./store`

## Run it

```console
$ xin build hello
✓ hello  realized  ck2q...  in main
→ .../results/hello
ok: 1 build run
$ cat results/hello/payload/greeting.txt
hello, xin
```

What happened:

1. xin hashed the node: its recipe bytes (plus the output-hashes of its
   inputs — none here) form the **input-hash**.
2. No store knew that input-hash, so the recipe ran — in a sandbox where
   the only writable places are `$XIN_PAYLOAD` (your result) and scratch.
3. The output tree was hashed into the **output-hash** and committed to
   `store/outputs/<hash>/`; the input→output mapping became a symlink in
   `store/inputs/`.
4. Because it is a target, `results/hello` now points at the output —
   and that symlink is what keeps it alive across `xin gc`.

## Rebuilds are free

```console
$ xin build hello
✓ hello  realized  ck2q...  in main
ok: 0 builds run
```

Same input-hash, mapping known, bytes present: nothing to do. Now edit
the recipe — even just the greeting text — and build again: the
input-hash changes, so a fresh build runs and lands under a *new*
output-hash. The old output is untouched (immutable, content-addressed);
`results/hello` is repointed. `xin gc` would reclaim the orphan.

## Poke around

```console
$ xin status hello        # present / needs-build, without building
$ xin log hello           # the build's stdout/stderr/exit
$ xin eval hello          # the TOML the Nickel evaluates to
$ xin store ls            # outputs and mappings in ./store
```
