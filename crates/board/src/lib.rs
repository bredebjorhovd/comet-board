//! comet-board — an autonomous-agent task board, ported from herdr-board.
//!
//! Linear issues (read-write) and GitHub issues/PRs (read-only by default) come
//! in; a dispatch releases a task into a comet chat with a coding agent; session
//! state reconciles back to the board and to the trackers.
//!
//! ## Provenance
//!
//! The tracker-facing core (`model`, `db`, `config`, `sources`, `stats`, `log`)
//! is ported from herdr-board with its herdr coupling removed. What herdr-board
//! called a *pane* is a comet *chat*: `Attempt::pane_id` stores a comet chat id.
//! The mapping between the two worlds lives in [`runtime`], and everything the
//! port deliberately did NOT bring along (screen scraping, nudge machinery,
//! delivery verification) is documented in `docs/BOARD.md`.

pub mod adopt;
pub mod config;
pub mod db;
pub mod dispatch;
pub mod doctor;
pub mod git_credentials;
pub mod init;
pub mod log;
pub mod model;
pub mod notify;
pub mod overrun;
pub mod review;
pub mod rows;
pub mod runtime;
pub mod settled;
pub mod sources;
pub mod stats;
pub mod sync;
