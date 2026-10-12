mod accounting;
mod anthropic;
pub mod attestation;
mod client;
pub mod compatibility;
pub mod config;
mod discovery;
mod error;
mod ledger;
pub mod moderation;
mod network;
pub mod oauth;
mod official;
pub mod policy;
mod provider;
mod reasoning;
mod resources;
pub mod router;
mod runtime;
mod scheduler;
pub mod server;
mod sse;
mod stream;
mod upstream;
pub mod vault;

pub use client::Client;
pub use error::Error;
pub use provider::Provider;
pub use serde_json::{Value, json};
pub use stream::CompletionStream;

// HTTP fixture suites run inside cfg(test); production binaries have no mock-host bypass.
#[cfg(test)]
extern crate self as teellm;
#[cfg(test)]
#[path = "../tests/completion.rs"]
mod completion_tests;
#[cfg(test)]
#[path = "../tests/router.rs"]
mod router_tests;
