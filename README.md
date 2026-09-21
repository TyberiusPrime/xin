# Xin - scientific, reproducable workflows, nix inspired

We are building a scientific, content-addressed build system, inspired by the
usual ilk (nix, bazel, buck), but with a different focus in the design space.


The overall goal is much 100% reproducibility,
no Merkle-tree-like-dependence of jobs on their non-immediate parents, 
easy sharing of output nodes (relocatable, not tied into /nix/store) 
and good provenance UX. 


Based on directed acyclic graphs (DAG)


# Crates:

- xin - the actual CLI
- xin-resolver the inner engine that decides what still needs to be done
- xin-dag - nickel based DAG definition layer
- xin-driver - the io layer.
