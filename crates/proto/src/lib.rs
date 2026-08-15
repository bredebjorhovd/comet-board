//! comet-proto — wire types shared by engine, UI, and RPC.
//!
//! Ported from comet's `packages/control/src/wire.ts` + `packages/harness/src/types.ts`.
//! Token-usage is never persisted into *docs* (a poor fit for CRDTs, and the original
//! exclusion): the `Usage` agent event stays a harness-level passthrough, kept in the host's
//! run journal and summed onto the board's own attempt rows for the stats page (gh#151).

pub mod agent;
pub mod context;
pub mod entities;
pub mod health;
pub mod motion;
pub mod view;

pub use agent::*;
pub use context::*;
pub use entities::*;
pub use health::EdgeHealth;
