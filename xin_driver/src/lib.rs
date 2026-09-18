//! xin_driver — the real-IO host for `xin_resolver`: filesystem local
//! stores (design.md B5 layout), a subprocess Process builder (B4-shaped
//! build dirs, no container yet), tree-hash output naming (B12 prototype
//! cut), garbage collection over gc-roots and leases, and a dummy remote
//! that never finds anything. DAG definitions come in via `xin_dag`.

pub mod builder;
pub mod driver;
pub mod gc;
pub mod local_store;
pub mod tree_hash;

pub use driver::{Backend, Driver, resolve, run_to_quiescence, run_with_policy};
pub use gc::{GcReport, run_gc};
pub use local_store::LocalStore;
