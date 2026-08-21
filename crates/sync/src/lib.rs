//! comet-sync — Loro room client (loro-protocol over WebSocket against the TS edge),
//! ephemeral presence, and the local `DocsStore` (SQLite snapshots + processed-command ledger).
//!
//! - [`RoomClient`]: joins a SessionRoom DO room (`wss://…/session/{chatId}/ws?token=`),
//!   backfills via version-vector diff, pushes local commits, imports remote updates,
//!   reassembles/produces fragments, relays `%EPH` presence, and reconnects with
//!   exponential backoff. Wire format is the official `loro-protocol` crate — byte-identical
//!   to the npm package the edge imports.
//! - [`DocsStore`]: snapshot persistence and the processed-command ledger, plus the durable
//!   half of [`convergence`] — the semantic outbox and the recovery quarantine. Consumers
//!   choose the safe claim boundary for their side effect; see `comet-engine`'s recoverable
//!   `Run` transition.
//! - [`churn`]: how often rooms JOIN AND THEN DIE, which is the one thing a
//!   point-in-time "is it connected?" census structurally cannot see (gh#527).
//!   Anything rendering an engine as healthy has to ask this too.
//! - [`convergence`]: the one module at the room-sync seam that may replace a document whose
//!   history the edge can no longer accept, and the only thing that puts locally committed
//!   transcript content back on top of the replacement (gh#483). Read it before touching
//!   reseed, replay, or anything that reports a room as converged.

pub mod churn;
pub mod convergence;
pub mod jitter;
mod recovery;
mod room;
mod store;
pub mod wake;

pub use churn::{ChurnCensus, RoomChurn};
pub use convergence::{
    ConvergenceError, ConvergenceJournal, ConvergenceRecovery, ConvergenceState, MemoryJournal,
    RecoveryPhase, RecoveryRecord, ReplayReport, SemanticKind, SemanticMutation,
    UNACKNOWLEDGED_ALERT,
};
pub use recovery::{
    SessionRecovery, SessionRecoveryError, SessionRecoveryManifest, build_session_recovery,
};
pub use room::{DocRecovery, RoomClient, RoomEvent, StaticUrl, SyncError, UrlProvider};
pub use store::{DocsStore, StoreError};
