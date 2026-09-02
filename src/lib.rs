//! ruzz as a library. The binary in `main.rs` is a thin CLI over this; the
//! split exists so benchmarks (and, one day, embedders) can drive the
//! engine directly instead of going through a spawned process.

pub mod activity;
pub mod analyze;
pub mod config;
pub mod dashboard;
pub mod field_meta;
pub mod import;
pub mod memory;
pub mod metrics;
pub mod params;
pub mod schema;
pub mod search;
pub mod server;
pub mod sort;
pub mod store;
