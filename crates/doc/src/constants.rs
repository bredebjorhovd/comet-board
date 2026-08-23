//! Constants carried over from comet `packages/session-doc/src/constants.ts`.
//! Per the original design these are starting points — re-measure with real heavy sessions.
//!
//! Only the constants the Rust side actually reads live here. The ones the
//! Session DO owns (retention, update-log compaction, flush cadence) are NOT
//! mirrored: their authoritative copy is `edge/src/session-doc/constants.ts`,
//! and a second copy that nothing reads is one that drifts (the Rust
//! `COMPACT_LOG_BYTES` said 8 MiB while the edge had moved to 2 MiB).

/// Max bytes for a single message entry before continuation splitting.
pub const MSG_INLINE_MAX: usize = 256 * 1024;
/// Host commits streamed assistant segments into the doc at this cadence (ms).
pub const STREAM_COMMIT_MS: u64 = 120;
/// Byte budget for the in-memory doc LRU on device backends.
pub const DOC_LRU_BYTE_BUDGET: usize = 80 * 1024 * 1024;
/// Number of trailing messages materialized into the tail sidecar.
pub const TAIL_MESSAGE_COUNT: usize = 64;
/// Terminal output batching cadence (ms).
pub const TERMINAL_OUTPUT_BATCH_MS: u64 = 12;
/// Default TTL for durable commands.
pub const COMMAND_DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Current session doc schema version (`meta.schemaVersion`).
pub const SESSION_SCHEMA_VERSION: u32 = 1;
