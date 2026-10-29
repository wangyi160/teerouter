mod accounting;
mod admission;
mod agent;
mod time;
pub mod attestation;
mod balances;
mod billing;
mod client;
pub mod compatibility;
pub mod config;
mod discovery;
mod error;
mod ledger;
mod merchants;
mod network;
pub mod oauth;
mod official;
pub mod policy;
mod pricing;
mod provider;
mod receipt_relay;
mod receipts;
mod refusal;
mod resources;
mod request_proof;
mod user_auth;
pub mod router;
mod runtime;
mod scheduler;
pub mod server;
mod sse;
mod upstream;
pub mod vault;

pub use client::Client;
pub use error::Error;
pub use provider::Provider;
pub use serde_json::{Value, json};

// HTTP fixture suites run inside cfg(test); production binaries have no mock-host bypass.
#[cfg(test)]
extern crate self as teellm;
#[cfg(test)]
#[path = "../tests/native_client.rs"]
mod native_client_tests;
#[cfg(test)]
#[path = "../tests/config_fixture.rs"]
mod config_fixture;
#[cfg(test)]
#[path = "../tests/router.rs"]
mod router_tests;
