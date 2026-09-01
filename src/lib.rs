//! terva Matrix connector library.
//!
//! The binary (`main.rs`) is a thin verb dispatcher over these modules:
//! the connproto wire layer (from the shared terva-sdk-rust crates,
//! re-exported under their historical names), config + lifecycle verbs
//! (config/setup), and the Matrix service (matrix, PLAN.md §6).

// matrix-sdk's deeply nested futures overflow the default trait-solver
// recursion limit when proving Send/Sync.
#![recursion_limit = "256"]

pub mod config;
pub mod matrix;
pub mod setup;

// The protocol layer extracted to terva-sdk-rust (its P1/P2): frame
// types, framing, and the serve loop — golden/replay conformance suites
// live with the SDK now; this repo keeps the binary-level smokes.
pub use terva_connproto as proto;
pub use terva_connsdk as serve;
pub use terva_wire as wire;
