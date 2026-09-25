//! Hosted post-processing coordinator, re-exported from the pure `corti-chat` crate.
//!
//! The engine moved out of the app so it builds and tests on every platform; app call sites keep
//! their `crate::postprocess::` paths through this shim.

pub(crate) use corti_chat::coordinator::*;
