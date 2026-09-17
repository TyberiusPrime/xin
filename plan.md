 We are building a scientific, content-addressed build system,
    inspired by the usual ilk (nix, bazel, buck), but with a
different focus in the design space.


# Design decisions

1.

We store each node by it's hashed output (=content addressed).

It depends on exactly the content-addresses of it's input nodes (
input-hash = hash-of-hashes).

This insulates nodes from their changes in their grand+-parents,
iff their parents do not change.

The trade-off is that we do not have the whole build graph
before we start, but we discover it incrementally,

2.

External data comes in via fixed output derivations, which we treat
as trust-on-first-use (TOFU). The TOFU hashes must be stored back into the
build configuration.


3.
Like nix we distinguish between built time and run-time dependencies.
The former (buildInputs) are the inputs you need to build a node's output.
The later is what the build node afterwards references.

We're going to be more explicit about the later than nix, requiring
the node-builders to explicitly declare them, in a folder full of symlinks,
instead of scanning-the-output-for-input-hashes.

4.
Hermetic builds in containers. Hardly a design decision, more an
essential requirement.


4.
We are going to have multiple stores at once.
A project local store, a shared store on the machine,
any number of remote hosted upstream stores.

Nodes can define which (machine-local) store they want to be stored in
afterwards.

5.
The file-system is the database, not the UX.

Node output is stored in store/outputs/<output-hash>.
Inputs go into symlinks store/input/<input-hash> -> store/output/<output-hash>
Metadata, build logs etc go into store/meta/by_input/<input-hash>
GC protection goes into store/gc-protect/...
We do not need store/runtime-inputs/<output-hash>, this is below 
store/outputs/<output-hash>

But we do need store/build-inputs/<output-hash>/<input-hash> (since we might 
arrive at the same output-hash multiple ways).

And we'll work very hard to not get a sqlite database in there.

File system folder times can be used - maybe not on the output nodes,
but on the others - to answer questions like 'when was this build'.

Build logs & statistics are also not considered part of the output

6.
Actual evaluation is a continuous backward forward dance through
the graph to discover all the input/output hashes.

7. 
Remote stores can supply 
- outputs (queried by output-hash)
- input-hash->output-hash mappings (queried by input-hash)

for TOFU nodes, we derive their input-hash from the url,
so remote stores can supply us with the input-output-mapping
and the content.

We have to consider singing here - the outputs are CAS,
and therefore self validating. I mean nix signing only 'buys'
you 'can use insecure transfer' like http...


8. 
Garbage collection like nix, symlinked GC roots
that we trace back. Easy for runtime-deps, more complicated 
for the build-deps (do we keep one? all?, decide later)

9.
We're going to leverage nix store paths for software initially.
No point in doing all the hard work just to get something of the ground.
A nix store input node get's converted into a (local) store
entry by copying & ammending with the runtime-input-symlinks.

10. 
Implementation of the resolver is in rust, and will be 
trait based, so we can mock every single part of this.

This is going to be the most complex part of the whole endeavor.

We start with a DAG of unnamed (=human named) but linked nodes.
Some of which are marked as 'must be realized at the end' (=targets).

We prune the graph down to the upstream closures of these targets,
then start filling in names from the TOFU roots.

We then go down, repeatedly querying the stores (local & remote)
for input->output mappings (and output presence), and then 
either building or copying from remote stores what we need.
But we can only build once all build-time dependencies are realized.
And after downloading, we can discover that we need runtime-dependencies
that we have skipped so far, since we already had names for them from the remote.
So the scope of targets expands.

At the end, we symlink the targets in an 'results' folder (with human names).

It's a whole dance, with lot's of (partial) failures that can occur, 
and this will require exhaustive enums and fuzzying to get robust.

11.
Primary interface will be a single 'xin' command with subcommands.

Subcommands will always have a primary json output, which
we then 'scope down' to human readable if --format=json is not set.

12. 
The intermediary format's going to be json, because
of all the tooling around it.




