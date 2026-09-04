#![allow(clippy::all)]

// Relay-backed scenarios live outside the Enkr package so production builds
// do not resolve or depend on the private enkr-syncd crate.
#[path = "../../tests/accounts.rs"]
mod accounts;
#[path = "../../tests/app_sync.rs"]
mod app_sync;
#[path = "../../tests/resilience.rs"]
mod resilience;
#[path = "../../tests/scale.rs"]
mod scale;
#[path = "../../tests/sync.rs"]
mod sync;

#[path = "../../src/showcase.rs"]
pub mod showcase;
