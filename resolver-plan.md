# Resolver plan

How to get from our vague [design](design.md)
to an actual, implemented, tested resolved.

No recapitulate: 
The resolver, implemented in Rust starts with a DAG
of human-named build nodes, some of which are marked as 'targets'.


It's end condition are 
- the target nodes being 'realized', i.e. their bytes on disk and their
  runtime-dependencies bytes on disk as well, or being marked as failures,
  with annotation as to the nature & node source of the failure.
- a fully hash named DAG - that is an input-hash-of-hashes and output-hash
  tuple for every node or a failure marker.


To start, the DAG get's pruned to those the targets and their upstream closures.
Then the input-hashes of zero-input-nodes are being calculated.
Next all defined stores are queried for input->output mappings & output availability.

We then 'node-by-node' go forward in the DAG to spread the input-output names, 
and backward whenever we discover that we need to realize a node, because
it's either a target, no store knows the input->output mapping, or it's within 
the run-time or build-time closure of another node that we need (recursive definition).

Side condition: topological-order preserving execution order differences 
must end up with the same end conditions.

## Glossary

- DAG node: A build recipe, tied to other DAG nodes (upstream=dependencies/downstream=dependents),
  initially with a 'human name'. Might be annotated with a target-store preference. 
- human names: the DAG node names given by our input. Confirms to /A-Za-z0-9_-/ .
- output-hash: content-address for the product of the build recipe. Includes the bytes generated,
  and the runtime-closure links in terms of other node's output-hashes.
- input-hash: a hash across 
    a) the output-hashes of input-nodes to a DAG node 
    b) node-recipe
- node-recipe: An opaque blob of bytes that the DAG producer gave the resolver to a) hash,
  and b) hand over to the builder
- store: For the purpose of the resolver, an abstract machine that can be queried
    for input->output mappings and availability of content addressed outputs.
- local store: A store (see above) on the local file system. 
    Here, availability == presence. At least one local store is write-able (=primary store) for 
    newly build or downloaded outputs and input->output mappings. That 
    store will also store symlinks into other local stores.
- remote store: A store (see above) on another machine across the network.
    > Availability here means we need to download the into a local store to make it 'present'
- present: A node is a local store.
- realized: Node is present & all it's run-time-closure nodes also present.
- run-time-closure: The transitive set of nodes that a node depends on after build. 
- build-time-closure: The set of nodes, and their run-time-closure) that a node needs to be build.


## Implementation notes

Event driven architecture. All IO trait abstracted for deterministic simulation testing. 


## Inputs

```rust

struct HumanName(String);
struct StoreName(String);

// dag nodes
struct Node {
    // human_name: String, encoded in the Input hashmap
    builder: BuilderType,
    recipe: Vec<u8>,
    target_store: StoreName,
    remotes: ValidRemoteStores,
    upstream_nodes: Vec<HumanName>
}

enum TargetStore {
    Primary,
    Other<Name>
}
enum ValidRemoteStores {
        None,
        All,
        Allow(Vec<StoreName>), // accept only these
        Deny(Vec<StoreName>), // accept all but these.
}

enum Store {
    LocalStore,
    RemoteStore,
}

// stores
struct LocalStore {
    path: PathBuf,
    writeable: bool,
}
struct RemoteStore {
    url: String,
}


struct Input {
    nodes: HashMap<HumanName, Node>,
    stores: HashMap<StoreName, Store>
}
```

### Runtime data
```rust
struct Hash {
    value: [u8;20], // nix has 160 bits of store hash... seems reasonably long.
    kind: HashKind,
}

enum HashKind {
    Blake3
}

struct InputHash([u8;32]);
struct OutputHash([u8;32]);


struct Runtime {
    input: Input,
    node_states: HashMap<String, NodeState> // by human name...
    stores: HashMap<String, StoreQuery>
    writeable_stores: HashMap<String, StoreWriter>
}

struct NodeState {
    input_hash: Option<InputHash>,
    output_hash: Option<OutputHash>,
    bytes_available: enum(NotYetKnown, BeingQueried, InLocalStore<StoreName>, 
                    BeingBuild, BeingDownloaded),
    runtime_closure_available: (Yes, Unknown, Missing<Vec<HumanName>)
    outcome: enum(Realized,  Named, Failed(NodeFailure))

}

enum NodeFailure {
    Unclassified,
    Upstream,
    TOFUMismatch,
    NonDeterminismDetected,
    Build(stdout, stderr, returncode, temp_build_path),
    Download(todo),
}

struct StoreQuery {
    known_mappings: Vec<(InputHash, OutputHash)>,
    available_outputs: HashSet<OutputHash>,
    query: Box<dyn QueryStore>,
}

```

### Interfaces 
```rust
// I'll use async to denote 'this goes into the execution pile and get's back
// with an event eventually. Not actually an async endorsement.

trait QueryStore
{
    fn query_inputs(input_hashes: Vec<InputHash>) -> Vec<QueryResult>

}
struct QueryResult {
    output_hash: OutputHash,
    present: bool,
}


enum BuilderType {
    Process,
    FetchURL,
    //...

}
//fetchers are also builders.
trait Builder {
    fn build_process(
        local_store: QueryStore,
        recipe: Vec<u8>,
    ) -> NodeBuildOutcome
    async fn build_fetchurl(
        local_store: QueryStore,
        recipe: Vec<u8>,
    ) -> NodeBuildOutcome
}

enum NodeBuildOutcome {
    OK{output_name, BuildLog{stdout, stderr, returncode}},
    Failed(NodeFailure),
}


```





