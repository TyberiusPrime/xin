 We are building a scientific, content-addressed build system, inspired by the
usual ilk (nix, bazel, buck), but with a different focus in the design space.



The overall goal is much 100% reproducibility,
no Merkle-tree-like-dependence of jobs on their non-immediate parents, 
easy sharing of output nodes (relocatable, not tied into /nix/store) 
and good provenance UX. 
Multi GB / TB data-sets must possible.

The concept is naturally split into multiple, 
interacting parts:

a) a DAG definition language
b) a 'resolver' 
c) actual containerized build.

The goal of this plan is to nail down the resolver

It is also intentionally vague in many of the decisions 
relating the other parts.

The goal today is to prototype / mock an evaluation engine,
and only once we got that solid with all the potential failure modes
worked out, we graduate to a real world store writing system.

# Open problems

The two big open problems are
- the resolving / evaluation algorithm. We can develop this completely independently.
- the relocability and conversely the access outside of the container

Here we tackle part one.

# Central assumptions:

Builds must be deterministic. And we help enforce that.
Computational non-determinism is anathema to reproducible analysis!

# Design decisions involving the resolver

## A1.

We store each node by it's hashed output (=content addressed).

Nodes are full folders, with a special structure
/payload - the output of whatever ran
/runtime-inputs/ - ../../<output-hash> symlinks.

Output hashes are done over the whole folder tree.
See below for details.

This is relevant in so far as that the resolver must 
produce exactly this information, payload, runtime-inputs, hash-of-them.

And the output-hash is used as the input to downstream nodes,
removing the Merkle-tree-like-dependence of nix.

This is common in scientific analysis, e.g. when a parameter change 
does not lead to an output change, or a software update does not affect
a particular run. 

We can't save having to recalculate / redownload node X once when it's inputs
change, but we can skip recalculating downstreams if it's outputs do not.

## A2

External data comes in via Fixed Output Derivations (FOD), which we treat as
trust-on-first-use (TOFU).

TOFU mismatches are immediate build failures. We keep the output (until next gc),
but do not update the input->output matching.

There is no 'no-input' node that is not a Fixed Output Derivation.

But the nodes are not just an url + the supposed output hash. 
They also need a fetcher (fetchurl, fetchzip, fetchFromGithub...)

 The TOFU hashes must be stored back into the build
configuration, so communicating them back to the (not yet designed) DAG
definition frontend is necessary. We might use a temporary lock file for this,
but the lock file won't be the user facing interface, nix makes the right 
choice of keeping the definitions and the hashes together.

## A3.
Like nix we distinguish between built time and run-time dependencies.
The former (buildInputs) are the inputs you need to build a node's output.
The later is what the build node afterwards references.

All the resolver cares about is that they get listed. 


## A4 
We are going to have multiple stores at once.
A project local store, a shared store on the machine,
any number of remote hosted upstream stores.

Nodes can define which (machine-local) store they want to be stored in
afterwards.

Remote stores can have different TOFU trust policies - only 
stores the user explicitly trusts / opts in on (think lab-shared
download proxies / data access systems) can provide TOFU input->output hashes,
which then becomes 'trust the remote store'.

That means for 'local remote store', we place 
symlinks in the local store. Yes, that affects relocability of 
those other remote stores. If that becomes a real world issue, we'll 
add in a 'rewrite symlinks that point to /some/other/store to /new/place/of/other/store'
command.


## A5
Remote stores (=not machine local) can supply 
- outputs (queried by output-hash)
- input-hash->output-hash mappings (queried by input-hash)

For TOFU nodes, we derive their input-hash from the url,
so remote stores can supply us with the input-output-mapping
and the content (if enabled for that remote store!).

Signing on the mappings would give us transport security,
but leave 'the remote store is the attacker' vector open.
The CAS outputs can be validated by hashing them, once you have a trusted
input-output mapping.

We need also a way to decide policy on 'remote stores disagree
on input->output mapping' - for the first version that's a build failure for that node,
and requires blacklisting it's input hash for one (or all) remote stores to get around.

##  A6

During build, we must tell the store which paths not to GC right now.

That means we need to protect them regardless of their existence, 
to not run into 'time of check, time of declaration, time of use'
discrepancies.

## A7
Implementation of the resolver is in Rust, and will be 
trait based, so we can mock every single part of this
 (store, builds, downloads...).

This is going to be the most complex part of the whole endeavor.

We start with a DAG of unnamed (=human named) but linked nodes.
Some of which are marked as 'must be realized at the end' (=targets).

At the end of the algorithm, the targets will be present in one of the local stores,
and their runtime-closures will be present, and we will have learned
the input->output mapping of every node in their build-time closure.
Or we have failures that prevent the targets from being build, 
and have causal annotation for what happened.

During the link naming, DAG entries might virtually collapse (due to same output hash),
but that can not introduce cycles, since we don't 'merge' the nodes - we just
treat both of them as 'present, output name is <hash>'.

So each node has at least these somewhat independent state axes:
(everything subject to more details...)

Named: not-named-yet, input named, output named
Bytes available: not-yet-known, In-local-store-<xyz>, being-build, being-downloaded,
Remote store availability: not-yet-known, output-hash available in which stores
Runtime-closure local available: yes, unknown, missing-the-following
outcome: Present. Named (presence not required). Failed (FetchFailed, BuildFailed,
tofu mismatch, upstream-failed <upstream-human-name>...)


To begin prune the graph down to the upstream closures of the targets,
then start filling in names from the roots. 

All roots without input are fixed output derivation. 
For TOFU that means we can discover their output names by downloading (if not yet stored).
For non-tofu that means we need them before hand.

We then go down, repeatedly querying the stores (local & remote)
for input->output mappings (and output presence), and then 
either building or copying from remote stores what we need.

But we can only build once all build-time dependencies are realized.
And after downloading, we can discover that we need runtime-dependencies
that we have skipped so far, since we already had names for them from the remote.
So the scope of targets to realize expands. 

This loop eventually terminates, since it's not generating new nodes, just pushing
existing nodes from 'we need to name this' into 'we need to realize this'. Limited
in scope.

We symlink the targets in an 'results' folder (with human names) as they 
are produced. (The out-of-scope DAG definition is responsible for not having
producing collisions in the human names - the resolver is free to 
apply it's failure policy if that happens)

It's a whole dance, with lot's of (partial) failures that can occur, 
and this will require exhaustive enums and deterministic simulation (+- fuzzying) to get robust.

Also we need to support at least two failure policies: keep-going & fail fast.
Remote retry configuration etc will be secondary. 
Same for 'should we keep partial result', we'll get there eventually.

## A8
Conflicting information from stores on input->output mapping: 
build failure. Ability to blacklist mappings for individual remote stores 
necessary. That allows local rebuild.

Even if the remote store is blacklisted, we should loudly log that, 
and record the discrepancy in the meta output.

## A9
Hashing: Input hash-of-hashes and output-hashes should be visible distinct.
We'll use the last byte appended to hashes for this, and encoding 
a hash version as well. Can't use lower/upper case bits (some file systems
are still case insensitive...), but that still means we get at least 32 letters
(RFC4648 base32 alphabet), so we can encode a hash-version/algorithm into that.

(Suffix instead of prefix since sharding would be greatly diminished.

Let's Tie this down:
B -> output hash, hashing version 1
A -> input-hashes,  hashing version 1

D -> output hash, hashing version 2 (which we don't have yet)
C -> input-hashes,  hashing version 2

(so 'input before output', later letters -> larger values),
and uppercase to visually split it from the actual hash.

## A10

Remote store queries.
Since we find things to query piece-by-piece, we need to avoid 
repeated requests. Initially, we can work with batching (the first
TOFU nodes will happen in quick succession), then we might consider
using HTTP/2 for it's streaming capability?

I don't think we can do speculative queries to the remotes though.

And since we record every mapping it's only going to be an issue on first
build, and scientific endeavor's are all about 'tweak parameters, run again'
anyway.

We also need 'negative-result caching' with TTL for the remotes, 
to prevent asking them repeatedly. And we should be able to
tell each DAG node what remote might actually have it - no point
querying 'software' upstreams for our specific datasets.


## A11 
The fuzzing must fuzz failing builders, downloaders etc.
Not sure about 'brown-out' downloads that go down to bytes/s, maybe detect them later
for now that's not an error case.


## A12
The resolver is 'state less' in the sense that every time it learns 
something, that leads to an immediate on-disk change. 
Resuming then means chasing up the graph again, but with locally stored
'known input-output-mappings' that's going to be fast.

(That also means we should store those in the local store, 
no matter whether the output-cas itself is in a remote store.
Copy them there when building the output-cas though).


## A13
Event driven design. Throwing out a build, by subprocess, or by 'submitting it 
to some kind of scheduler' should be supported.

## A14
Are multiple outputs per node worth it?
They do complicate the resolver, and they can be simulated 
by the upstream creating one node that writes everything, and then 
downstreams that symlink into that.
So: No, one output per node. 


## C1

Input hashing.
Use Blake3 for now.

Briefly, the idea is that they depend on (the-hash-of-) a mapping 
{name => output-hash-of-input-node}, and 
a second mapping with 'special inputs'.

Special inputs are e.g. the build script,
the build command, env variables, maybe architecture,
for TOFU: url & fetcher.

For the resolver, the special inputs are a an opaque value,
while the input's output-hashes come resolving upstream nodes.

For TOFU nodes the fetcher will be such a special input.
The URL will be a 'special' input as well - because 'changed the URL but left 
the TOFU hash the same' is one of the most annoying nix footguns. 

Format:
Simple line based format, with separator between the mappings.
mapping names sorted.
Format:
```
output-hash:nameA
output-hash:nameB
--
value-hash:hash-of-buildScript
value-hash:hash-of-env_vars
```
env_vars: sort, stringify (bash syntax), hash.
other special inputs: hash their bytes.


This insulates nodes from their changes in their grand+-parents,
iff their parents do not change.

The trade-off is that we do not have the whole build graph
named before we start, but discover what nodes correspond to which output folders
and input->output mappings incrementally.

# Design decisions that the resolver itself does not care about.

## B1
Node layout:

Secondary lint to verify runtime inputs - we need symlinks to be 
relative ../runtime-inputs/name, or ../../<output-hash>, not 
/xin/<whatever>.

To make this more robust, we'll randomize
the '/xin' part during builds. This helps with finding accidentally
embedded absolute paths.

Nodes depend on a yet to be formalized description of their inputs.


## B2

We're going to be more explicit about runtime-references than nix, requiring
the node-builders to explicitly declare them, in a folder full of symlinks,
instead of scanning-the-output-for-input-hashes.

## B3
Hermetic builds in containers. Hardly a design decision, more an
essential requirement.

Sandbox for now unspecified, no network for non fixed-output/TOFU nodes.


## B4
Inside the containers, we have a virtualized file system below /xin
the build scripts use.
/xin/out/payload
/xin/out/runtime-inputs
/xin/<cas-hash> (the inputs to this build)
/xin/inputs/by-name/<name> (relative symlinks into the previous)

Relocability is going to be one major advantage vs nix,
so we need the runtime-inputs so the build scripts
can actually declare - I mean we can scan for ../../<cas-hash>, 
possibly, and complain if they're absolute, but I think we should have this
at least as an escape hatch.


## B5
The file-system is the database, not the UX.

It is also the source of truth - any index we might need must
be derivable & disposable.

Node output is stored in store/outputs/<output-hash>.
Inputs go into symlinks store/input/<input-hash> -> store/output/<output-hash>

Metadata, build logs etc go into store/meta/by_input/<input-hash>
(store/meta/by_output/<output-hash> can either link to a by_input meta if build,
or log what store we got it from).

GC protection goes into store/gc-protect/, which symlinks to other folders,
which symlink to store folders that are protected, just like nix.

We do not need store/runtime-inputs/<output-hash>, this is below 
store/outputs/<output-hash>

But we do need store/build-inputs/<output-hash>/<input-hash>/<name> symlinks
(since we might arrive at the same output-hash multiple ways).

Hashes in file systems should be sharded by their first two characters/bytes.

File system folder times can be used.
Not inside the output nodes, these obviously need fixed file times
for reproducibility, but on the others, the data-symlinks they'll serve 
just fine to answer questions like 'when was this build'.
(Should duplicate that in meta-data though, just to be robust).

Build logs & statistics are also not considered part of the output.

Builds happen in store/temp/<mkdtemp name> and get placed into their final destination atomically by rename,
after canonicalization. ENOTEMPTY is ok, that's a parallel build that was
faster. Don't forget fsyncs. needs a reaping policy for crashed builds?

Failed builds stick around in a special store folder for inspection
- maybe for a couple of invocations, but no longer than the next gc?

Concurrent builds are additive, worst case is a wasted compute.
(If your jobs need 10 hours of compute to find you did them twice 
in different graphs: You need to split your jobs better).


## B6.
Shared (machine local) stores are trusted by default, users must share group,
files must be og=rX. Store files ain't writeable, but we don't 
do nix level remounting-to-prevent-even-root-writes.

Those would be discovered by verification runs (see B18).
Same with bad CAS-content in shared stores.

The most common use case is single-user-cross-project 
shared data/indices anyway.



## B7
Garbage collection like nix, symlinked GC roots (may be outside of the store)
that we trace back. Easy for runtime-deps, more complicated 
for the build-deps (do we keep one? all?, decide later).

Meta GC separate policies. Same for failed outputs.

During build, keep a temporary gc root so gc can not collect things
we're currently using. Nix maybe also scan's /proc

## B8
We're going to leverage nix store paths for software initially.
No point in doing all the hard work just to get something of the ground.

It's a special builder (so not the regular containerized script),
in which nix store input node get's converted into a (local) store
entry by copying into /payload & ammending with the runtime-input-symlinks
(which is a query to 'nix path-info --json')

(That means we need a preprocessing step that expands a nix store-path
into the local DAF of it's runtime closure, and then they get 'build'/imported step by step).

And it also means that nix-store-import is a special kind of build, not containerized,
nor TOFUed, since it needs to see /nix/store from the outside world.

We then map these back into our containers at /nix/store .

Note that the references might only work inside the container, 
outside of the container nix might GC those paths. Won't affect our builds
- we got our copy, but needs special handling during export.

## B9
Primary interface will be a single 'xin' command with subcommands.

Subcommands will always have a primary JSON output (JSONL for streams) , which
we then 'scope down / format prettily' to human readable if --format=json is not set.
(ie. 'human readable' is a subset/transformation of the json output)

Sub command set undefined at this time.
Build interface should offer tui level introspection, not just
'everything/one line' options like nix.

## B10
The intermediary format's going to be json, because
of all the tooling around it.


## B11
Parallelism; Node build needs either one or all cores (defined, not discovered).
Schedule accordingly. We'll eventually need concurrent download policies etc
but not for the prototype.

Changing the number of cores is not allowed to change output.
It not an invariant! 
If the scientific software can't handle that it's not fit for purpose,
and needs either post-processing or fixing until it does.

Temp store for build is in the target store. We know which store that is at build time.

Secondary limits: timeouts, resource limits, also definable, but not invariants,

## B12

Output hashes are build from the whole tree. Disallow non-utf8 filenames,
case-sensitivity colissions, device/socket/fifo etc. No permission bits but exec. 
Blake3 as a hash algorithm? Should parallelize well?

For serialization, Crib as much from NAR as possible.
Hard-links (duplicate? or special note.), sparse-files (not uncommon in scientific datasets): hash
logical content ('doh'), record hole map? needs research

"Single-byte-change in TB dataset" isn't particularly relevant, 
the hashes here are more about provenance then efficiency.

Needs a literature search maybe there's a highly unpacked byte identical archive format 
nowadays.

Hashes are encoded in padding-less RCF4648 Base32, lowercased, with
an additional version/type suffix (see A9).


## B13
No-copy ingestion of datasets. Somewhat similar to our nix-store ingestion.
One the one hand, trivial, just place a symlink in the store instead of a folder,
and bind mount them into the containers.

Problem: provenance, hash tracking, protection / detection of changed files?
I mean , inode + timestamp + size go far and should protect against all
accidental changes (which then lead to TOFU failure and build abort).
But we need to store them somewhere. Active manipulation we can't defend against anyway.

(This isn't meant for NFS/Lustre style sharing. Those systems have much less
of an inode concept, and mtime granularity is bad. Will it still work if we just
track timestamp & size. Probably. But advise users against this, I suppose).

What about 'changing during build'. It's a niche case, could be caught with
before/after hashing. Then what, tofu failure? We'd notice on a rerun 
anyway, because we log the *before* inode/timestamp/size, and then rehash.
So non issue in practice I believe.


## B14
Cross-store dangling closures

GC roots must be able to cross stores, so that GC in 'shared' local stores
has a chance. But GC in non-local store is an explicit op, we don't go 
cleaning up stores as a suprise.


## B15
We will need a 'export everything into a new store, ready for reproduction'
command eventually. But on the store / resolver side this is 
just symlink chasing & copying it all together.

Tying it together with the DAG creating code is outside of today's scope.

(no-copy-ingestion nodes need to be copied in as well).


## B16
History is just a number of GC roots and maybe a special policy

## B17.
Not deciding what language the build scripts are in right now. That's 
a post resolver decision. 

## B18
We need commands for validation-by-rebuild, validation-against-remote-store
and similar to help the rebuilding.
I'd also envision a chaos monkey style approach where we 'randomly' rebuild
nodes in the graph (up to a maximum 'it takes this much extra time per build'
policy) for verification.


## B20
Viewing outside of containers is unsolved right now. We might do something creative
with FUSE, or looka ta Spack and conda (shudder)

## B21
S3 and stuff. TOfU nodes, local copy, GC policy. Nothing special beyond
mayhaps a fetcher.


# Prototype development


The heart & core is of course the state machine per node, and the
set of events, and the 'drive evaluation forward' algorithm.

Mocking every IO with traits, and having complex enough mockers
should allow us to use deterministic simulation testing 
in addition to individual unit & behavior tests.

This is vital, our last voyage into this space (pypipegraph2) nearly
failed because it was so difficult to get the evaluation right.
We're in better shape here (on 'temporary' jobs, better concept), 
but wary.

Crates to consider for deterministic simulation testing madsim, turmoil.
Perhaps proptest for DAG generation. 




