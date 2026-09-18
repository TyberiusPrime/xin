//! xin_driver — the real-IO host for `xin_resolver`: filesystem local
//! stores (design.md B5 layout), the mandatory build sandbox (bubblewrap +
//! bootstrap busybox, B4 /xin layout — there is no unsandboxed build
//! path), tree-hash output naming (B12 prototype cut), garbage collection
//! over gc-roots and leases, and a dummy remote that never finds anything.
//! DAG definitions come in via `xin_dag`.

pub mod builder;
pub mod container;
pub mod driver;
pub mod gc;
pub mod local_store;
pub mod tree_hash;

pub use container::{Bwrap, Sandbox};
pub use driver::{Backend, Driver, resolve, run_to_quiescence, run_with_policy};
pub use gc::{GcReport, run_gc, runtime_closure};
pub use local_store::LocalStore;
