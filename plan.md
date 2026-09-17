 We are building a scientific, content-addressed build system, inspired by the
usual ilk (nix, bazel, buck), but with a different focus in the design space.

This is the 'evaluate DAG into build nodes' half.
DAG specification is out of scope, will be designed at a later date.


The goal is much 100% reproducibility,
no Merkle-tree-like-dependence of jobs on their non-immediate parents, 
easy sharing of output nodes (relocatable, not tied into /nix/store), 
good provenance UX. Multi GB / TB datasets possible.


It is also intentionally vague in many of the decisions 
relating to the outside computing landscape. 
The goal today is to prototype / mock an evaluation engine,
and only once we got that solid with all the potential failure modes
worked out, we graduate to a real world store writing system.

# Open problems

The two big open problems are
- the resolving / evaluation algorithm. We can develop this completely independently.
- the relocability and conversely the access outside of the container



# Design decisions

1.

We store each node by it's hashed output (=content addressed).

Nodes are full folders, with a special structure
/payload - the output of whatever ran
/runtime-inputs - symlinks to in-container folders

Output hashes are done over the whole folder tree.
See below for details.

Secondary lint to verify runtime inputs - having the runtime-input path
written down somewhere without runtime-inputs is a build failure
(should this be opt-out?)

Nodes dependson exactly the content-addresses of it's named input nodes (
input-hash = hash-of-hashes). There is one 'special' input,
the build-script. (possibly there will be more special inputs, such as the
container system used). Env Vars, parameters are part of the build script.

This insulates nodes from their changes in their grand+-parents,
iff their parents do not change.

The trade-off is that we do not have the whole build graph
before we start, but discover it incrementally,

2.

Builds must be deterministic.

3.

External data comes in via fixed output derivations, which we treat
as trust-on-first-use (TOFU). The TOFU hashes must be stored back into the
build configuration, so communicating them back to the (not yet designed) frontend
is necessary.

TOFU mismatches are immediate build failures. We keep the output (until next gc),
but do not update the input->output matching.



4.
Like nix we distinguish between built time and run-time dependencies.
The former (buildInputs) are the inputs you need to build a node's output.
The later is what the build node afterwards references.

We're going to be more explicit about the later than nix, requiring
the node-builders to explicitly declare them, in a folder full of symlinks,
instead of scanning-the-output-for-input-hashes.

5.
Hermetic builds in containers. Hardly a design decision, more an
essential requirement.

Sandbox for now unspecified, no network for non fixed-output/TOFU nodes.

6.
We are going to have multiple stores at once.
A project local store, a shared store on the machine,
any number of remote hosted upstream stores.

Nodes can define which (machine-local) store they want to be stored in
afterwards.

Remote stores can have different tofu trust policies - only 
stores the user explicitly trusts / opts in on (think lab-shared
download proxies / data access systems) can provide TOFU input->output hashes,
which then becomes 'trust the remote store'.

7.
Inside the containers, we have a virtualized file system below /xin
the build scripts use.
/xin/out/payload
/xin/out/runtime-inputs
/xin/inputs/by-hash/<cas-hash>
/xin/inputs/by-name/<name> (symlinks into the previous)

The runtime-input symlinks must be into /xin/inputs/by-hash.
and we need to scan/lint and consider it a build failure if anything 
inside payload references /xin/inputs/by-hash

What about references to /xin/out/payload though?
I had in mind to use a fuse to serve 'store adjusted' paths later on,
but this is essentially an open problem.

Even relative links won't save us, since runtime-input nodes may be in
'shared' stores' (we even might have multiple local stores).

Relocability is going to be one major advantage vs nix,
so we need to think long and hard about this.


8.
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

Builds happen in store/temp/<pid> or similar, and get 
placed into their final destination atomically by rename.


Failed builds stick around in a special store folder for inspection
- maybe for a couple of invocations, but no longer than the next gc?

Concurrent builds are additive, worst case is a wasted compute.
(If your jobs need 10 hours of compute to find you did them twice 
in different graphs: You need to split your jobs better).

9.
Shared stores are trusted by default, users must share group,
files must be og=rX. Store files ain't writeable, but we don't 
do nix level remounting-to-prevent-even-root-writes.

10. 
Remote stores can supply 
- outputs (queried by output-hash)
- input-hash->output-hash mappings (queried by input-hash)

for TOFU nodes, we derive their input-hash from the url,
so remote stores can supply us with the input-output-mapping
and the content.

signing on the mappings would give us transport security,
but leave 'the remote store is the attacker' vector open.
The CAS outputs can be validated by hashing them, once you have a trusted
input-output mapping.

We need also a way to decide policy on 'remote stores disagree
on input->output mapping' - for now that's a build failure for that node
, and requires blacklisting it's input hash for one (or all) remote stores.



We have to consider singing here - the outputs are CAS,
and therefore self validating. I mean nix signing only 'buys'
you 'can use insecure transfer' like http...

11. 
Garbage collection like nix, symlinked GC roots (may be outside of the store)
that we trace back. Easy for runtime-deps, more complicated 
for the build-deps (do we keep one? all?, decide later).

Meta GC separate policies. Same for failed outputs.

During build, keep a temporary gc root so gc can not collect things
we're currently using. Nix maybe also scan's /proc

12.
We're going to leverage nix store paths for software initially.
No point in doing all the hard work just to get something of the ground.

A nix store input node get's converted into a (local) store
entry by copying into /payload & ammending with the runtime-input-symlinks
(which is a local scan during the 'build' step, not a global scan-for-references
mechanism).

(That means we need a preprocessing step that expands a nix store-path
into the local dag of it's runtime closure, and then they get 'build'/imported step by step).

And it also means that nix-store-import is a special kind of build, not containerized,
not TOFUed, since it needs to see /nix/store from the outside world.

We then map these back into our containers at /nix/store .

13. 
Implementation of the resolver is in Rust, and will be 
trait based, so we can mock every single part of this.

This is going to be the most complex part of the whole endeavor.

We start with a DAG of unnamed (=human named) but linked nodes.
Some of which are marked as 'must be realized at the end' (=targets).

At the end of the algorithm, the targets will be present in one of the local stores,
and their runtime-closures will be present, and we will have learned
the input->output mapping of every node in their build-time closure.

So an individual node has states: 'Unresolved -> Resolved (=output-hash known) ->
Present (name known, bytes on disk) -> Realized (previous + run-time-closure also realized)'.

During the link naming, DAG entries might virtually collapse (due to same output hash),
but that can not introduce cycles, since we don't 'merge' the nodes - we just
treat both of them as 'present, output name is <hash>'.

To begin prune the graph down to the upstream closures of the targets,
then start filling in names from the roots. 
All roots without input are TOFU, 
which means we can discover their output names by downloading (if not yet stored).

We then go down, repeatedly querying the stores (local & remote)
for input->output mappings (and output presence), and then 
either building or copying from remote stores what we need.
But we can only build once all build-time dependencies are realized.
And after downloading, we can discover that we need runtime-dependencies
that we have skipped so far, since we already had names for them from the remote.
So the scope of targets to realize expands. 
This loop eventually terminates, since it's not generating new nodes.


We symlink the targets in an 'results' folder (with human names) as they 
are produced. (The out-of-scope DAG definition is responsible for not having
producing collisions in the human names - the resolver is free to 
apply it's failure policy if that happens)

It's a whole dance, with lot's of (partial) failures that can occur, 
and this will require exhaustive enums and fuzzying to get robust.

Also we need to support at least two failure policies: keep-going & fail fast.
Remote retry configuration etc will be secondary. 
Same for 'should we keep partial result', we'll get there eventually.

14.
Primary interface will be a single 'xin' command with subcommands.

Subcommands will always have a primary json output, which
we then 'scope down / format prettily' to human readable if --format=json is not set.
(ie. 'human readable' is a subset/transformation of the json output)

Sub command set undefined at this time.
Build interface should offer tui level introspection, not just
'everything/one line' options like nix.

15. 
The intermediary format's going to be json, because
of all the tooling around it.


16. 
Conflicting information from stores on input->output mapping: 
build failure. Ability to blacklist mappings for individual remote stores 
necessary. That allows local rebuild.

17.
Parallelism; Node build needs either one or all cores (defined, not discovered).
Schedule accordingly. We'll eventually need concurrent download policies etc
but not for the prototype.

Changing the number of cores is not allowed to change output.
It not an invariant! 

Temp store for build is in the target store. We know which store that is at build time.

Secondary limits: timeouts, resource limits, also definable, but not invariants,


18. 
Hashing: Input hash-of-hashes and output-hashes should be visible distinct
(maybe lowercase/uppercase)?. 

Output hashes are build from the whole tree. Disallow non-utf8 filenames,
case-sensitivity colissions, device/socket/fifo etc. No permission bits but exec. 
Crib as much from NAR as possible.
Blake3 as a hash algorithm? Should parallelize well?
Open qustions: Hard-links (duplicate), sparse-files (not uncommon in scientific datasets).

"Single-byte-change in TB dataset" isn't particularly relevant, 
the hashes here are more about provenance then efficiency.

19. 
No-copy ingestion of datasets. Somewhat similar to our nix-store ingestion.
One the one hand, trivial, just place the right mount in /xin/inputs/by-hash.
Problem: provenance, hash tracking, protection / detection of changed files?
I mean , inode + timestamp + size go far and should protect against all
accidental changes (which then lead to TOFU failure and build abort).
But we need to store them somewhere. Active manipulation we can't defend against anyway.

What about 'changing during build'. It's a niche case, could be caught with
before/after hashing. Then what, tofu failure? We'd notice on a rerun 
anyway, because we log the *before* inode/timestamp/size, and then rehash.
So non issue in practice I believe.

20. 
Cross-store dangling closures

GC roots must be able to cross stores, so that GC in 'shared' local stores
has a chance. But GC in non-local store is an explicit op, we don't go 
cleaning up stores as a suprise.

21.
Remote store queries.
Since we find things to query piece-by-piece, we need to avoid 
repeated requests. Initially, we can work with batching (the first
TOFU nodes will happen in quick succession), then we might consider
using HTTP/2 for it's streaming capability?

I don't think we can do speculative queries to the remotes though.

And since we record every mapping it's only going to be an issue on first
build, and scientific endeavor's are all about 'tweak parameters, run again'
anyway.

22. 
We will need a 'export everything into a new store, ready for reproduction'
command eventually. But on the store / resolver side this is 
just symlink chasing & copying it all together.
Tying it together with the DAG creating code is outside of today's scope.


23. 
History is just a number of GC roots and maybe a special policy - not in scope of resolver.

24.
Not deciding what language the build scripts are in right now. That's 
a post resolver decision. 

25. 
The fuzzing must fuzz failing builders, downloaders etc.
Not sure about 'brown-out' downloads that go down to bytes/s, maybe detect them later
for now that's not an error case.
