# 03 — a TOFUed input

`upstream/dataset.csv` plays the role of a file on the internet. The
`dataset` node fetches it; `summary` (the target) processes it.

```console
$ xin build fetch
✓ dataset  realized ...
✓ summary  realized ...
ok: 2 builds run
$ cat results/summary/payload/summary.txt
3 samples, 3498 reads total
```

## Trust on first use

A fetch node's input-hash covers the *URL*, not the content — the
content is exactly what xin cannot know in advance. So the first fetch
is trusted, and its output-hash is recorded: the pin.

Watch the pin hold. Edit the "upstream":

```console
$ echo 'mut_2,1005' >> upstream/dataset.csv
$ xin build fetch
ok: 0 builds run
$ cat results/summary/payload/summary.txt
3 samples, 3498 reads total
```

Nothing re-fetched, nothing changed: the mapping is known and the bytes
are in the store. Your analysis does not silently drift because someone
re-uploaded a file. That is the point of TOFU.

## Taking a new version — deliberately

The pin is tied to the recipe. Want the new data? Say so in the DAG —
point the recipe at a new URL (`.../dataset_v2.csv`): new input-hash,
fresh fetch, fresh pin, and both versions coexist in the store under
their own hashes.

## The mismatch detector

If the pinned mapping exists but the bytes are gone (a synced store, a
partial gc, disk surgery), xin must re-fetch — and it then *checks* the
result against the pin. Different bytes: the build fails with
`TofuMismatch`, and the recorded mapping is left untouched. xin never
quietly re-pins; a changed upstream is either a decision you make in the
DAG, or an error in your face.

```console
$ xin log dataset          # the fetch's log, like any build
$ xin status fetch
```

Undo the experiment: `git checkout -- upstream/dataset.csv`.
