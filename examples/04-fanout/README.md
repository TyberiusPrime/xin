# 04 — a generated fan-out

The point of using Nickel instead of static TOML: DAGs are values you
compute. This example builds `shards = 3` worker nodes with a fold, and
*generates the merge node's recipe* — one line per shard — the same way.

```console
$ xin dag fanout
  0 dataset
  1 shard-0  ← dataset
  2 shard-1  ← dataset
  3 shard-2  ← dataset
  4 merged [target]  ← shard-0, shard-1, shard-2
$ xin build fanout
ok: 5 builds run
$ cat results/merged/payload/merged.txt
shard 0: n=2 sum=2205
shard 1: n=2 sum=2560
shard 2: n=2 sum=1978
```

Bump `shards` to 4 and rebuild: `dataset` is untouched (same input-hash,
served from the store), the four new shard nodes and the new merge
build. Shard boundaries changed, so the shard *contents* changed —
content addressing keeps exactly what can be kept, no more.

## Inspecting generated DAGs

When the definition is code, look at what it evaluates to:

```console
$ xin eval fanout           # the TOML intermediary, fully expanded
$ xin eval fanout --format json | jq '.nodes | keys'
```

The TOML intermediary is a real interchange format: hand-written TOML
loads the same way (`xin build something.toml`), and the Nickel layer is
"just" a generator for it.

A note on quoting: recipes are opaque strings to Nickel, so `%{...}` is
Nickel interpolation while `%%` is a literal percent for awk. When a
recipe gets hairy, generate *less* of it — or make the script a fetched
input and keep the recipe to one `exec` line.
