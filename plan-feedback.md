# Feedback on plan.md — what stands between this and an implementable spec

(Rev 2, after discussion. Dropped: the anonymous-closure-node claim — wrong,
see the invariant note in blocker 2 for what it collapsed into; the
A8-only-covers-remotes claim — misread; most of the C1 complaints — special
inputs are opaque to the resolver, those concerns move to the DAG layer.)

Overall shape critique still stands: the stated goal is "nail down the
resolver," yet most of the text is store layout (B-sections) declared out of
resolver scope, while the resolver core gets ~40 lines. The state machine is
the deliverable of the prototype — the plan doesn't need to contain it, but
it does need to define what "done" looks like for it. Invert the word-count
ratio.

---

## Blockers

### 1. Define "done" for the state machine (A7)

Producing the state machine is the point of the prototype — fine. But
"solid with all the potential failure modes worked out" has no acceptance
criteria. Name it milestone 1 with concrete artifacts:

- enumerated per-node states (real enum variants, incl. the substituted-
  from-remote path),
- enumerated events (build finished, download failed, mapping learned,
  negative store answer, TOFU mismatch, mapping conflict, ...),
- a transition table where every (state, event) pair is either handled or
  explicitly declared a bug,
- the driver-loop invariants written down.

"Everything subject to more details" is the sentence to retire.

### 2. State and validate the invariant: runtime-inputs ⊆ build-inputs

The plan's soundness rests on an unstated chain: computing any node's
input-hash requires the output-hashes of its full build-time closure
(top-down, by induction), and a build can only declare runtime refs to
inputs it actually saw under /xin — so every runtime ref inside any
substituted output resolves to an already-named DAG node, no scope
explosion, and A7's termination argument holds. Two actions:

- Write that argument into the plan (three sentences); it's the actual
  reason "the loop terminates" is true, and the current justification
  ("not generating new nodes") only works because of it.
- Have the resolver *validate* it on substitution: the output CAS hash can
  be verified, but the mapping is only as good as the store's trust level.
  A trusted-but-wrong mapping can deliver an output whose runtime-inputs
  point at hashes outside the node's build closure. That must be a defined
  validation failure (same family as TOFU mismatch), not undefined behavior.

### 3. Define resolver behavior when a determinism violation is *detected*

Accepted: determinism is the user's responsibility, statistically checked.
Not covered: what the resolver does at the moment a divergence surfaces,
and it can surface in at least two places already in the design:

- B5's concurrent-build story only handles the same-output case
  (ENOTEMPTY on rename). Different-output case: both renames succeed into
  different store/outputs/ entries, then the store/input/<input-hash>
  symlink creation collides with a *different target*. That EEXIST is your
  divergence detector — define what it does.
- B18 validation-by-rebuild disagrees with the stored mapping.

By analogy with A2/A8 the answer is presumably "build failure + loud record
in meta," but write it, including the one hard part: what it means for
downstreams already realized against the earlier mapping (nothing? flag in
meta? that's a decision, not research — just make it).

### 4. The resolver's ingest boundary is undefined

The DAG language is out of scope; the resolver's *input types* are not.
Define the structs the resolver receives: node with opaque special-input
bytes, edges (build-time deps), runtime-dep declarations if separate,
target markers, fetcher id for FODs, per-node store hints (A10), per-node
target store (A4), core count (B11). Without this, no line of the prototype
— mocked or not — can be written.

### 5. Close the remaining TOFU/mapping seams (A2, A12)

A8's conflict policy covers store-vs-store, local included — fine. Still
open:

- A12 says the store is the source of truth for learned mappings; A2 says
  TOFU hashes get written back into the build configuration (via lock file,
  then into definitions). Two places, one truth — define precedence when
  they disagree (stale definition hash vs. store mapping is exactly the nix
  footgun class you cite).
- "We keep the output (until next gc) but do not update the mapping" —
  under what key does that orphaned output live and how is it found for
  inspection?
- "There is no 'no-input' node that is not a FOD" reads wrong against C1,
  where every node has special inputs (script, env). Presumably you mean
  "no node without *node* inputs escapes FOD treatment unless it has a
  build" — say it precisely; this is glossary material.

### 6. The concurrency / GC protocol is vibes

"Don't forget fsyncs" and "needs a reaping policy?" are reminders, not a
design. A6 correctly identifies the check/use race and then doesn't give
the mechanism. Needed:

- gc-protect entry lifecycle: creator, lease semantics (PID/heartbeat?
  lockfile?), crash reclamation, and the ordering that makes "protect
  regardless of existence" race-free against concurrent GC.
- the atomic commit sequence for a finished build: temp → fsync → rename
  output → create input→output symlink → write meta, failure behavior at
  each step. Note the different-target EEXIST case belongs to blocker 3 —
  design them together.

---

## Items to push to the DAG-definition layer (so they don't get lost)

These left the resolver's plate in review but must land on someone's:

- Canonical env-var encoding for the special-input hash — "bash syntax" is
  not canonical (many valid quotings). Define one encoding.
- Architecture in or out of the special inputs — decide; if out, two
  architectures share an input-hash and will trip blocker 3's detector.
- C1 name-mapping format: prohibit newlines in names explicitly, specify
  the sort key (byte-wise on name?). ':' is fine — fixed-length hash comes
  first.

---

## Contradictions / sharp edges (reduced list)

- **A4 cross-store symlinks.** State explicitly that runtime-input symlinks
  hash by literal link content and that resolution is a store-layer
  concern. The "symlink in local store → other store" mechanism makes
  store/outputs/ heterogeneous (dirs + out-of-store symlinks); GC tracing,
  verification (B18), and export (B15) each need a sentence saying how they
  treat those entries.
- **B8 nix imports.** Store relocation is fine (container re-maps
  /nix/store) and the plan already flags export. Remaining ask: outputs
  whose payloads carry /nix/store refs are consumable only through the
  container mapping — record that per-node (a taint bit in meta) so export
  and any future outside-container access (B20) can report it instead of
  the consumer discovering dangling refs.
- **Provenance vs. early cutoff.** One output reachable from multiple
  input-hashes (B5's build-inputs multimap admits it) means provenance
  queries return alternatives, not a single chain. Fine — but say it; it
  shapes the meta schema and the UX promised in the goals.

---

## Underspecified — write these down before coding

- **Failure taxonomy.** The plan promises "exhaustive enums" and names ~5
  failure kinds in passing. Enumerate for real; keep-going policy is
  meaningless until you know what each failure poisons downstream.
- **keep-going semantics.** Exit status for partially realized targets;
  schema of the "causal annotation" (chain of (node, failure) pairs?).
- **Cycle detection.** "We start with a DAG" — enforced by whom? One
  sentence: resolver validates, or resolver trusts the frontend.
- **Store targeting.** Nodes choose their store (A4), temp lives in the
  target store (B11). Constrain explicitly: build targets are machine-local
  stores only; remote stores are substitution/publish-only.
- **Where blacklists and negative caches live.** A8's per-remote blacklist
  and A10's TTL cache are state; "the filesystem is the database" (B5), so
  give them paths and formats.
- **A13 is one sentence.** Specify what crosses the driver/executor
  boundary or fold it into A7.

---

## The prototype section needs milestones

No trait list, no acceptance criteria. Suggested shape:

1. **M1** — types + the state machine on paper: Node, NodeState, Event,
   transition table, invariants; Store/Builder/Fetcher traits. This is
   blocker 1's definition of done.
2. **M2** — driver loop, single in-memory store, no failures.
3. **M3** — failure injection through mocks; exhaustive (state × event)
   coverage; keep-going + fail-fast.
4. **M4** — multiple stores: substitution, runtime-closure validation
   (blocker 2), mapping conflicts + blacklists, negative caching.
5. **M5** — deterministic simulation (madsim/turmoil) with the headline
   property stated explicitly: *for a fixed DAG and fixed mock-world
   behavior, final store state and learned mappings are identical under
   every interleaving.* This property is nowhere in the plan and is the
   entire reason to do DST — put it in.
6. **M6** — proptest DAG generation + fuzzed failure schedules (A11).

Structural suggestion stands: B-sections are decision records, A/C sections
want to be a spec. Split — keep plan.md as ADRs, extract resolver-spec.md
containing blockers 1–6 resolved, and write the prototype against that.

---

## Nits

- Typos throughout ("relocability", "colissions", "fuzzying", "ammending",
  "DAF", "suprise", "RCF4648", "much 100% reproducibility"). Notes-grade is
  fine; spec-grade isn't, once others implement against it.
- A9: add one sentence that the marker letter's *case* carries no
  information on case-insensitive filesystems — position does — so nobody
  "fixes" it later.
- A10: batching is the decision; the HTTP/2 speculation can go.
- B12 sparse/hardlink research doesn't block the resolver prototype
  (hashing is mocked) — mark it post-prototype so it doesn't read as a
  blocker.
- Glossary: node, input (node-input vs special-input!), name, input-named,
  output-named, realized, substituted, mapping, target, closure. Half the
  review disagreements above were really about these words.

---

## What's right (keep, don't re-litigate)

- Early cutoff via output-hash substitution — the differentiator, correct
  for scientific workloads.
- Top-down naming discipline that makes the closure argument work
  (blocker 2) — it's *right*, it's just unstated.
- One output per node (A14); explicit runtime refs over scanning (B2);
  filesystem as truth with disposable indices (B5); trait-mocked IO + DST
  as the test strategy; suffix-encoded hash versioning (A9).

## Order of operations

1. Glossary.
2. Decide the two conflict policies (blockers 3 and 5) — decisions, not
   research.
3. Write the closure/termination invariant + substitution validation into
   A7 (blocker 2).
4. Define the ingest types (blocker 4).
5. M1: state machine on paper with the acceptance artifacts (blocker 1).
6. Milestones M2–M6; push the DAG-layer items to that layer's plan so they
   aren't orphaned.
