//! An indexer for `IdentityNames` events.
//!
//! One binary, two halves: a polling loop that mirrors the contract's storage
//! into Postgres from its events alone, and a read API that answers
//! resolution (handle -> wallet, account id -> wallet, wallet -> identities)
//! and partial-handle search.

#![deny(missing_docs)]

pub mod api;
pub mod chain;
pub mod cli;
pub mod db;
pub mod events;
pub mod indexer;
pub mod nodes;
