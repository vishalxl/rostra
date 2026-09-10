//! Pure, experimental payload retention policy; never authorizes deletion.
//!
//! Callers must independently enforce content lifecycle/projection safety.
//! These types perform no I/O and neither prune payloads nor enable a runtime
//! worker.

mod distance;
mod policy;
pub mod simulation;

pub use distance::RetentionDistance;
pub use policy::{RetentionKey, RetentionPolicy};

#[cfg(test)]
mod tests;
