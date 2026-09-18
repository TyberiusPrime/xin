//! xin_driver — the real-IO host for `xin_resolver`: filesystem local
//! stores (design.md B5 layout), a subprocess Process builder (B4-shaped
//! build dirs, no container yet), tree-hash output naming (B12 prototype
//! cut), and a dummy remote that never finds anything. The DAG definition
//! layer is still missing, so `RawInput` is handed over programmatically.

pub mod builder;
pub mod driver;
pub mod local_store;
pub mod tree_hash;

pub use driver::{Backend, Driver, resolve, run_to_quiescence};
pub use local_store::LocalStore;
