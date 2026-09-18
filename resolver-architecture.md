# Architecture recommendations for the event-driven resolver

Input: [design.md](design.md), [resolver-plan.md](resolver-plan.md).
Scope: the shape of the code, not the policy decisions (those are settled in A1–A14).

---

## 1. Key the state by three different things, not just by node

`resolver-plan.md` has one table, `node_states: HashMap<HumanName, NodeState>`.
That collapses three facts with three different natural keys:

| fact | natural key | why not the node |
|---|---|---|
| "this recipe resolves to that output" | `InputHash` | two human-named nodes can be byte-identical recipes; they must not both build |
| "these bytes are present locally, closure and all" | `OutputHash` | A7's DAG collapse: distinct nodes can share an output; one download, one lease |
| demand, blame, human-facing status | `NodeId` | attribution is per node |

Recommended core:

```rust
pub struct Resolver {
    dag:          Dag,                                   // immutable after prune
    nodes:        IndexVec<NodeId, NodeSlot>,            // demand + blame + pointers
    mappings:     BTreeMap<InputHash, MappingState>,     // naming / building
    realizations: BTreeMap<OutputHash, RealizationState>,// presence / download
    knowledge:    StoreKnowledge,                        // positive+negative cache, blacklists
    inflight:     BTreeMap<RequestId, InflightKind>,
}
```

The decisive argument is runtime-closure expansion. When a substituted output
arrives, its `runtime-inputs/` are **bare output-hashes**; the corresponding DAG node
may be one we never needed to name (it was cut off early). With realization keyed by
`OutputHash`, those need no node identity at all — they enter `realizations` as plain
work items and the loop is unchanged. With realization keyed by node, you need a
reverse output-hash→node index that is not guaranteed to exist, and the "scope of
targets expands" paragraph in A7 turns into a special case.

`NodeSlot` then gets thin, which is the point — it becomes demand propagation and
attribution, and the interesting machines are the two hash-keyed ones.

## 2. Pure core: events in, effects out, no async anywhere near it

`resolver-plan.md` writes the traits as `async fn build_process(...) -> Outcome`.
Recommend not doing that, even as notation. Make the core a sync fold:

```rust
impl Resolver {
    pub fn start(&mut self) -> Vec<Effect>;
    pub fn apply(&mut self, ev: Event) -> Vec<Effect>;   // total, no IO, no clock, no rng
    pub fn quiesced(&mut self) -> Outcome;               // no inflight, no ready work
}
```

Everything IO-ish is an `Effect` the host hands to an executor; every executor reply is
an `Event`. Benefits, in order of importance:

- Deterministic simulation needs no runtime at all: a test is `Vec<Event>` → assert.
  **You can defer madsim/turmoil entirely**; they earn their place only once real
  network IO exists under the executors. A hand-rolled scheduler that pops from the
  pending-effect set by seeded RNG gives stronger control (it can reorder completions
  that a real runtime would never reorder) and shrinks better under proptest.
- The transition table (M1 in the feedback doc) is literally `apply`'s match arms, so
  "every (state, event) pair handled or declared a bug" is checkable by a test that
  enumerates the product.
- Parallelism policy (B11), batching (A10), retries, and rate limits live in the host
  scheduler where they can change without touching resolver semantics.

Traits then look like fire-and-forget with a correlation token:

```rust
trait StoreQuery  { fn query(&mut self, tok: RequestId, req: QueryBatch); }
trait StoreWriter { fn commit(&mut self, tok: RequestId, req: CommitRequest); }
trait Builder     { fn start(&mut self, tok: RequestId, req: BuildRequest); }
trait Clock       { fn now(&self) -> Instant; }
```

Note `Builder` is **one** method taking `BuilderType` as data. `resolver-plan.md` has
`build_process` and `build_fetchurl` as separate trait methods; adding a fetcher should
not change a trait that every mock implements.

## 3. Make illegal states unrepresentable; keep demand orthogonal

`NodeState` currently has five independent axes (`input_hash`, `output_hash`,
`bytes_available`, `runtime_closure_available`, `outcome`), i.e. a product where most
combinations are nonsense (`bytes_available = BeingBuild` with `input_hash = None`).
Fold each machine into one enum where the data lives in the variants:

```rust
enum MappingState {
    Unresolved,                                       // input-hash known, nothing asked yet
    Querying { pending: StoreSet, answers: Vec<(StoreName, OutputHash)> },
    Resolved(OutputHash),
    Building  { lease: LeaseId },
    Failed(FailureId),
}

enum RealizationState {
    Absent,
    Available { in_stores: StoreSet },                 // remote-only
    Downloading { from: StoreName, lease: LeaseId },
    Present { store: StoreName, runtime: ClosureState },
    Failed(FailureId),
}
```

Demand is genuinely orthogonal to progress, so keep it as its own field and make it
**monotone** — demand is added, never retracted:

```rust
struct Demand { name: bool, realize: ReasonSet }   // Target | BuildInputOf(NodeId) | RuntimeOf(OutputHash)
```

Storing *why* something is demanded is what makes the keep-going report explicable and
gives you the cycle-free argument for free.

## 4. State the whole thing as a monotone fixpoint — it buys you the side condition

`resolver-plan.md` asserts "topological-order-preserving execution order differences
must end up with the same end conditions". That is confluence, and it is not automatic;
it is automatic if every learned thing is a *fact* on a finite join-semilattice and no
transition ever removes one. Concretely, adopt three rules:

1. Facts (`InputHash→OutputHash`, `OutputHash present in store S`, `node failed`) are
   inserted, never mutated. A second, different value for an existing fact is not an
   overwrite — it is the A8 conflict detector firing.
2. Demand only grows.
3. `apply` is a function of `(state, event)` only — no clock reads, no RNG, no
   environment. Timeouts and TTLs enter as `Event::Timer` produced by the host.

Then termination is: finite node set × finite lattice height × every event either raises
a value or is a no-op. That is the argument A7 gestures at, and it is worth writing as a
test: a debug-mode `assert!(new_state >= old_state)` on a hand-written `PartialOrd`.

**Determinism hygiene** is the other half, and it is where this class of engine actually
breaks:

- Never iterate a `HashMap` in the core. `BTreeMap`/`IndexMap`, or iterate a `Vec<NodeId>`.
  Intern `HumanName` to `NodeId(u32)` at prune time and use names only for output.
- When a node could be blamed on several failed upstreams, pick deterministically
  (lowest `NodeId`), not "whichever event arrived first". Same for "which store do we
  download from" when several have it — a stable ranking, tie-broken by store name.
- One seeded `Rng` behind a trait, used only by the host (chaos rebuilds B18, jitter).

## 5. Batch store queries at quiescence, not on a timer

A10 wants batching without speculative queries. The clean trigger is the driver loop's
own shape: drain the event queue completely, *then* flush. No timers, no latency
heuristics, fully deterministic:

```rust
loop {
    while let Some(ev) = events.pop() { effects.extend(resolver.apply(ev)); }
    if effects.is_empty() && inflight.is_empty() { break; }
    scheduler.dispatch(coalesce(effects.drain(..)));   // per-store query merge here
    events.extend(executors.collect_ready());
}
```

`coalesce` merging `QueryStore` effects per store is a host concern; the core emits one
intent per input-hash and stays simple. The first TOFU wave batches naturally because
those events all land in one drain pass.

Negative cache and per-remote blacklist (A8, A10) belong in `StoreKnowledge`, consulted
*before* an effect is emitted, so "don't ask twice" is an invariant of the core rather
than of the executor. Give it a file format now (B5 says the filesystem is the database);
TTL expiry arrives as `Event::Timer`.

## 6. Nail the commit points; everything else is fire-and-forget

A12 ("stateless, learns→writes immediately") does not require a WAL, and building one
would be the wrong trade. The correct rule is asymmetric:

- **Losing** a learned fact on crash is harmless — re-derivation is a store query.
- **Recording a wrong fact** is fatal.

So: persistence effects for meta, logs, and caches are fire-and-forget; only the two
real commit points are ack-gated, i.e. the node does not reach `Present`/`Resolved`
until the ack event arrives:

1. `rename(temp → outputs/<oh>)` — `ENOTEMPTY` means a concurrent build won; treat as success.
2. `symlink(input/<ih> → outputs/<oh>)` — `EEXIST` with a *different* target is the
   nondeterminism detector (feedback blocker 3). It must be a distinct `Event`, not an
   `io::Error` string.

Model the GC lease (A6) as an explicit state with its own effect pair
(`AcquireLease`/`ReleaseLease`) keyed by hash, acquired *before* the build/download
effect is emitted and released on terminal state. Making it a state rather than an RAII
guard is what lets the simulator run GC in the middle of a build and assert nothing is
collected.

## 7. Failures: one table, id-referenced, with keep-going as a scheduler policy

```rust
struct FailureRecord { kind: FailureKind, origin: NodeId, detail: FailureDetail }
```

Nodes/mappings hold a `FailureId`; the causal chain is *reconstructed* by walking
`origin` at report time rather than stored per node (otherwise the chain is duplicated
and can disagree with itself).

Keep-going vs fail-fast should be a **scheduler** policy — "stop dispatching new
effects, drain inflight" — not a second code path in the core. This matters for the DST
property, which needs splitting in two because fail-fast is legitimately not confluent:

- keep-going: *final fact set and store state are byte-identical under every interleaving.*
- fail-fast: *the fact set is a subset of the keep-going run's, and no fact contradicts it.*

Write both; the second is the one that will actually catch bugs, because it is easy to
accidentally let fail-fast record something the full run never would.

## 8. Validate the substitution invariant where the events arrive

The soundness argument is runtime-closure ⊆ build-closure. Give it teeth at the one
place it can be violated — a mapping from a store you trust that delivers an output
whose `runtime-inputs/` point outside the node's build closure. Keep, per node awaiting
substitution, the union of its build inputs' runtime closures, and check the arriving
output against it; a miss is `FailureKind::ClosureEscape`, same family as TOFU mismatch,
and it is also the natural place to attribute blame to the offending store.

## 9. Testing architecture

- **Transition table test.** `apply` returns `Result<_, Unhandled>` in test builds;
  a test enumerates `StateKind × EventKind` and asserts each is handled or listed in an
  explicit `IMPOSSIBLE` table with a one-line reason. This is M1's acceptance artifact.
- **World mock, not per-trait mocks.** One `SimWorld` owning stores/builders/clock/rng
  with a scripted failure schedule; the traits are thin views onto it. Per-trait mocks
  drift out of agreement about what exists.
- **Schedule as data.** `struct Schedule { seed: u64, choices: Vec<u8> }` drives which
  inflight request completes next and which failures inject. proptest shrinks that to a
  minimal reordering, which is the difference between a usable and a useless failure report.
- **Model check the small cases.** For DAGs ≤ 5 nodes, enumerate *all* interleavings
  exhaustively instead of sampling. Most confluence bugs are reachable at n=3.

## 10. Concrete corrections to the types in resolver-plan.md

- `Input` has no `targets` field. Nothing in the file marks a node as a target.
- `struct Hash { value: [u8;20] }` vs `InputHash([u8;32])` — pick one. Blake3 is 32;
  if you want nix's 160 bits, truncate deliberately and say so. The A9 type/version
  suffix should live in the type and in `Display`, not be appended at print sites.
- `enum TargetStore` is defined but `Node.target_store` is a `StoreName`; and A4's
  constraint ("build targets are machine-local stores only") should be a parse-time
  check, not a runtime one.
- `upstream_nodes: Vec<HumanName>` is insufficient for C1 hashing: the input mapping is
  `{input-name => output-hash}` and the *local alias* a recipe uses need not equal the
  upstream's human name. Make it `Vec<(InputName, HumanName)>`.
- `QueryStore::query_inputs(...) -> Vec<QueryResult>` drops the correlation (which
  result is for which input-hash) and cannot express "not known". Return
  `Vec<(InputHash, Option<QueryResult>)>`, or better a `BTreeMap`.
- `NodeBuildOutcome::OK { output_name, .. }` should carry the output *hash* plus the
  declared runtime references — the resolver needs both, and the second is what feeds §8.
- Per-node core count (B11) and per-node remote hints exist in design.md but not in `Node`.
- Cycle detection: decide explicitly whether the resolver validates or trusts the
  frontend. Recommend validating during the prune — it is ~10 lines with petgraph and it
  is the difference between a clean error and a hang.

---

## Suggested build order

1. Types + `Event`/`Effect` enums + the three tables. No logic.
2. `apply` for the happy path, one in-memory store, no failures (feedback M2).
3. Transition-table test harness + failure injection (M3).
4. Multi-store: substitution, closure validation (§8), conflicts, negative caching (M4).
5. Confluence properties + exhaustive small-DAG model check (M5).
6. proptest DAG generation, then real executors behind the same traits (M6).
