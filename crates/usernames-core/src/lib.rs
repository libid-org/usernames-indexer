//! The read model behind the usernames indexer and its read API.
//!
//! Two binaries stand on this crate and neither contains logic of its own:
//! `usernames-indexer` runs [`indexer`], the polling loop that mirrors
//! `IdentityNames` storage into Postgres from its events alone, and
//! `usernames-api` serves [`api`], which answers resolution (handle ->
//! wallet, account id -> wallet, wallet -> identities) and partial-handle
//! search over what the loop wrote.
//!
//! They are separated because they scale and fail differently: one writer per
//! chain holds a lease, while readers are stateless and horizontal. The halves
//! stay here together because they share the read model and because the
//! end-to-end test drives both — the split is a deployment boundary, not a
//! dependency one.

#![deny(missing_docs)]

pub mod api;
pub mod chain;
pub mod db;
pub mod ens;
pub mod events;
pub mod indexer;
pub mod nodes;
