mod hashes;
mod inputs;
use std::{collections::HashSet, process::Output};

use hashes::{InputHash, OutputHash};
use indexmap::IndexMap;

use crate::inputs::{NodeId, StoreName};

struct Resolver {
    input: inputs::Input,
    state: IndexMap<NodeId, (Demand, MappingState, RealizationState)>,
    failures: Vec<Failure>
}
impl Resolver {
    pub fn new(input: inputs::Input) -> Resolver {
        Resolver {
            input: input.prune_to_targets(),
        }
    }

    // find the first things we need to do.
    // Not an event due to it's singualr nature
    pub fn start(&mut self) -> Vec<Effect> {}

    // we learned something! Now let's see i
    pub fn apply(&mut self, ev: Event) -> Vec<Effect> {
        match ev {}
    }

    fn state(&self) -> _ {
        self.state
    }
}

//indices into the failures table
struct FailureId(u32);
//to associate buidls back to what we're doing...
struct LeaseId(u32);

enum MappingState {
    Unresolved,                                       // input-hash known, nothing asked yet
    Querying { pending: HashSet<StoreName>, answers: Vec<(StoreName, OutputHash)> },
    Resolved(OutputHash),
    Building  { lease: NodeId }, 
    Failed(FailureId),
}

enum RealizationState {
    Absent,
    Available { in_stores: StoreSet },                 // remote-only
    Downloading { from: StoreName, lease: LeaseId },
    Present { store: StoreName, runtime: ClosureState },
    Failed(FailureId),
}

enum Failure {
    Upstream,
    BuildFailed,
    DownloadFailed,

}
struct Demand {
    name: bool, //isn't that true for everything though?
    realize: HashSet<DemandReason>
}

enum DemandReason {
    Target,
    BuildInputOf(NodeId),
    RuntimeOf(OutputHash),

}


struct NameDiscovered(InputHash, OutputHash);

/// What the external (io) system can tell the resolver.
enum Event {
    NameDiscovered(NameDiscovered),
    BytesNowInStore(NodeId, OutputHash),
    BuildFailed(NodeId, BuildError),
    DownloadFailed(NodeId, BuildError),
}

struct BuildError {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    return_code: i8, 
    // and so on.
}

/// What the resolver needs the external system to work on and report back.
enum Effect {}

#[cfg(test)]
mod test {

    struct MockSimulator {

    }


}
