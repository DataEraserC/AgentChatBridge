#![cfg(feature = "wasm-host")]
//! WASM 宿主：wasmi 运行时 + 一条极小的模块 ABI。

pub mod runtime;

pub use runtime::{Decision, DecisionOutput, WasmHost};
