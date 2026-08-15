//! Durable command ledger — port of `packages/session-doc/src/commands.ts`.
//!
//! Rules (verbatim from comet's design):
//! 1. Each device inserts only its own entries; entries are append-only and immutable.
//! 2. The chat's HOST is the sole writer of command outcomes; a composer may only set
//!    `cancelled` on its own still-pending entries.
//! 3. Evaluation (`evaluate_command`, pure): processed-id dedupe → Skip; expired TTL → Expired;
//!    a newer command of the same kind supersedes steer/interrupt; an interrupt whose
//!    `based_on.turn_id` is already past → Superseded; otherwise Execute.
//!
//! gh#424 adds the follow-up queue to that ledger rather than beside it. Two
//! kinds carry it — [`SessionCommandPayload::Queue`], one queued instruction,
//! and [`SessionCommandPayload::QueueControl`], one mutation of the visible
//! plan — and the plan itself is *derived* by folding them ([`crate::queue`]),
//! never stored. Rule 1 is what forces that shape: entries are immutable, so
//! "edit the third follow-up" cannot be a write to the third follow-up; it is a
//! new entry that the fold reads. What it buys is exactly what the durability
//! primitive already gives every other command — two clients converge because
//! they fold the same list, an offline viewer sees the plan because the plan is
//! the doc, and a restart loses nothing because nothing was in memory.
//!
//! Two evaluation outcomes exist only for this (see [`CommandDisposition`]):
//! `Defer` — a follow-up whose turn has not come — and `Cancelled` — one the
//! plan no longer contains. `Defer` is the load-bearing one: it is the single
//! rule that makes "do this after the turn" different from "steer this turn",
//! and it is why a queued entry must NOT be marked processed when the drain
//! passes over it.

use serde::{Deserialize, Serialize};

use comet_proto::{ContextRef, RunRequest, UserInputAnswer};

use crate::constants::COMMAND_DEFAULT_TTL_MS;
use crate::queue::QueueView;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandKind {
    Run,
    Steer,
    Interrupt,
    RespondInput,
    /// A follow-up: the next instruction, to run after the live turn settles.
    Queue,
    /// A mutation of the queued plan (edit / move / remove / clear / pause).
    QueueControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCommandStatus {
    Pending,
    Applied,
    Rejected,
    Expired,
    Superseded,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SessionCommandPayload {
    #[serde(rename_all = "camelCase")]
    Run {
        request: RunRequest,
        /// Client-minted message id for the optimistic user entry (dedup key).
        message_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steer {
        prompt: String,
        message_id: Option<String>,
    },
    Interrupt {},
    #[serde(rename_all = "camelCase")]
    RespondInput {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    /// A follow-up instruction: run it as its own turn once the live one
    /// settles. Never merged into the running turn (that is `Steer`) and never
    /// superseded by a later one (that is the point of a plan).
    #[serde(rename_all = "camelCase")]
    Queue {
        prompt: String,
        /// Client-minted message id for the user entry this becomes, so the
        /// echo of a follow-up dedupes exactly like a send's.
        message_id: String,
    },
    /// A mutation of the queued plan. Immutable like every other entry: what
    /// changes is what [`crate::queue::project`] computes from the log.
    QueueControl {
        op: QueueOp,
    },
}

/// One mutation of the plan.
///
/// Every op names its target by command id and is idempotent under replay: an
/// edit sets a value, a remove is a tombstone, a move re-anchors. That is what
/// lets two clients apply them in different orders locally and still agree —
/// the fold runs over the doc's converged entry order, not over the order
/// either client happened to see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum QueueOp {
    /// Rewrite a queued follow-up's prompt (and the references it carries).
    #[serde(rename_all = "camelCase")]
    Edit {
        target: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        context: Vec<ContextRef>,
    },
    /// Place `target` immediately after `after`, or at the head when it is
    /// `None`. Anchored to a neighbour rather than to an index because indices
    /// mean different things to two clients holding different-length views, and
    /// an anchor that has since been removed simply falls back to the tail.
    #[serde(rename_all = "camelCase")]
    Move {
        target: String,
        #[serde(default)]
        after: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Remove { target: String },
    /// Drop everything queued *before this entry* — a later follow-up is not
    /// retroactively cleared by an earlier clear.
    Clear {},
    /// Hold the plan: the host stops dequeuing, and says so.
    Pause {},
    Resume {},
}

impl SessionCommandPayload {
    pub fn kind(&self) -> SessionCommandKind {
        match self {
            SessionCommandPayload::Run { .. } => SessionCommandKind::Run,
            SessionCommandPayload::Steer { .. } => SessionCommandKind::Steer,
            SessionCommandPayload::Interrupt {} => SessionCommandKind::Interrupt,
            SessionCommandPayload::RespondInput { .. } => SessionCommandKind::RespondInput,
            SessionCommandPayload::Queue { .. } => SessionCommandKind::Queue,
            SessionCommandPayload::QueueControl { .. } => SessionCommandKind::QueueControl,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandBasedOn {
    pub turn_id: Option<String>,
    pub frontier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommandEntry {
    pub id: String,
    pub payload: SessionCommandPayload,
    pub issued_by: String,
    /// Epoch millis.
    pub issued_at: i64,
    #[serde(default)]
    pub based_on: Option<CommandBasedOn>,
    /// Epoch millis; defaults to issued_at + COMMAND_DEFAULT_TTL_MS when absent.
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub status: SessionCommandStatus,
    #[serde(default)]
    pub resolution: Option<String>,
    /// Typed `@` references this instruction was written against (gh#424),
    /// checkout-relative and resolved by the host at execute time.
    ///
    /// On the entry rather than inside each payload variant for the same reason
    /// `based_on` is: it describes the instruction, not the verb, and every verb
    /// that carries a prompt can carry references. Serde-defaulted, so an entry
    /// written by an engine that predates this reads back as one with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextRef>,
}

impl SessionCommandEntry {
    pub fn kind(&self) -> SessionCommandKind {
        self.payload.kind()
    }

    /// When this command stops being worth executing — `None` for one that
    /// never does.
    ///
    /// The TTL exists so a send that reached nobody does not surprise somebody
    /// hours later. A *queued* follow-up is the opposite case: it was written to
    /// wait, the thing it waits on is a turn that may legitimately run for
    /// hours, and expiring it would delete the user's plan for the crime of the
    /// agent being slow. Its mutations (`QueueControl`) inherit that for the
    /// same reason — an edit that expired while the host was offline would
    /// resurrect the text it replaced.
    pub fn effective_expiry(&self) -> Option<i64> {
        match self.kind() {
            SessionCommandKind::Queue | SessionCommandKind::QueueControl => None,
            _ => Some(
                self.expires_at
                    .unwrap_or(self.issued_at + COMMAND_DEFAULT_TTL_MS),
            ),
        }
    }
}

/// Rule 2: only the composer that issued a still-pending command may cancel it.
pub fn can_composer_cancel(entry: &SessionCommandEntry, device_id: &str) -> bool {
    entry.status == SessionCommandStatus::Pending && entry.issued_by == device_id
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDisposition {
    /// Already in the processed ledger — do nothing (idempotence).
    Skip,
    /// Mark expired.
    Expired,
    /// Mark superseded.
    Superseded,
    /// Execute. The host chooses the durable transition: arbitrary side
    /// effects are marked first, while a `Run` stays recoverably Pending until
    /// the sessions engine owns its live handle.
    Execute,
    /// A follow-up whose turn has not come: leave it PENDING, do not mark it
    /// processed, move on to the next entry (gh#424).
    ///
    /// The one disposition that does not resolve anything. Every other outcome
    /// is terminal for its entry, which is why the drain may mark processed
    /// before acting on it; a deferred entry will be evaluated again — after
    /// this turn, after a reconnect, after a restart — and marking it processed
    /// would mean the plan silently evaporating.
    Defer,
    /// A follow-up the plan no longer contains (removed, or cleared): mark it
    /// cancelled. Written by the HOST, like every other outcome — a client
    /// removing a row appends the control entry and the fold hides the row at
    /// once, but the entry's own status is not a client's to write (rule 2).
    Cancelled,
}

/// Context the host evaluates a pending command against.
pub struct EvaluationContext<'a> {
    /// Processed-command ledger membership test.
    pub is_processed: &'a dyn Fn(&str) -> bool,
    /// Current wall clock, epoch millis.
    pub now_ms: i64,
    /// All command entries in doc order (used to find newer same-kind entries).
    pub entries: &'a [SessionCommandEntry],
    /// The id of the turn currently (or most recently) running, if any.
    pub current_turn_id: Option<&'a str>,
    /// True when the given turn id has already completed.
    pub turn_is_past: &'a dyn Fn(&str) -> bool,
    /// The plan, folded from the same `entries` ([`crate::queue::project`]).
    pub queue: &'a QueueView,
    /// Is a turn live on this chat right now? A follow-up waits for this to be
    /// false; nothing else consults it.
    pub chat_is_running: bool,
}

/// Rule 3 — pure evaluation of a single pending command.
pub fn evaluate_command(
    entry: &SessionCommandEntry,
    cx: &EvaluationContext<'_>,
) -> CommandDisposition {
    if (cx.is_processed)(&entry.id) {
        return CommandDisposition::Skip;
    }
    if entry
        .effective_expiry()
        .is_some_and(|expiry| cx.now_ms >= expiry)
    {
        return CommandDisposition::Expired;
    }
    // A follow-up is dispatched by the plan, not by its own arrival order.
    if entry.kind() == SessionCommandKind::Queue {
        let Some(position) = cx.queue.position(&entry.id) else {
            return CommandDisposition::Cancelled; // removed, or cleared
        };
        if cx.queue.paused.is_some() || cx.chat_is_running || position > 0 {
            return CommandDisposition::Defer;
        }
        return CommandDisposition::Execute;
    }
    // A newer pending command of the same kind supersedes steer/interrupt.
    let kind = entry.kind();
    if matches!(
        kind,
        SessionCommandKind::Steer | SessionCommandKind::Interrupt
    ) {
        let has_newer_same_kind = cx.entries.iter().any(|other| {
            other.id != entry.id
                && other.kind() == kind
                && other.status == SessionCommandStatus::Pending
                && other.issued_at > entry.issued_at
        });
        if has_newer_same_kind {
            return CommandDisposition::Superseded;
        }
    }
    // An interrupt aimed at a turn that already finished is moot.
    if kind == SessionCommandKind::Interrupt
        && let Some(based_on) = &entry.based_on
        && let Some(turn_id) = &based_on.turn_id
    {
        let is_current = cx.current_turn_id == Some(turn_id.as_str());
        if !is_current && (cx.turn_is_past)(turn_id) {
            return CommandDisposition::Superseded;
        }
    }
    CommandDisposition::Execute
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, payload: SessionCommandPayload, issued_at: i64) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: "device-a".into(),
            issued_at,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
            context: Vec::new(),
        }
    }

    fn steer(id: &str, issued_at: i64) -> SessionCommandEntry {
        entry(
            id,
            SessionCommandPayload::Steer {
                prompt: "go".into(),
                message_id: None,
            },
            issued_at,
        )
    }

    fn queued(id: &str, issued_at: i64) -> SessionCommandEntry {
        entry(
            id,
            SessionCommandPayload::Queue {
                prompt: "then this".into(),
                message_id: format!("m-{id}"),
            },
            issued_at,
        )
    }

    fn cx<'a>(
        entries: &'a [SessionCommandEntry],
        processed: &'a dyn Fn(&str) -> bool,
        turn_is_past: &'a dyn Fn(&str) -> bool,
        now_ms: i64,
        current_turn_id: Option<&'a str>,
    ) -> EvaluationContext<'a> {
        EvaluationContext {
            is_processed: processed,
            now_ms,
            entries,
            current_turn_id,
            turn_is_past,
            queue: &EMPTY_QUEUE,
            chat_is_running: false,
        }
    }

    /// The queue-free context every pre-gh#424 rule is evaluated in.
    static EMPTY_QUEUE: std::sync::LazyLock<QueueView> = std::sync::LazyLock::new(QueueView::default);

    const NEVER: fn(&str) -> bool = |_| false;

    #[test]
    fn processed_commands_are_skipped() {
        let e = steer("c1", 1_000);
        let entries = vec![e.clone()];
        let processed = |id: &str| id == "c1";
        let cx = cx(&entries, &processed, &NEVER, 2_000, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Skip);
    }

    #[test]
    fn expired_commands_are_expired() {
        let e = steer("c1", 0);
        let entries = vec![e.clone()];
        let cx = cx(&entries, &NEVER, &NEVER, COMMAND_DEFAULT_TTL_MS + 1, None);
        assert_eq!(evaluate_command(&e, &cx), CommandDisposition::Expired);
    }

    #[test]
    fn newer_steer_supersedes_older_pending_steer() {
        let older = steer("c1", 1_000);
        let newer = steer("c2", 2_000);
        let entries = vec![older.clone(), newer.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&older, &cx1),
            CommandDisposition::Superseded
        );
        assert_eq!(evaluate_command(&newer, &cx1), CommandDisposition::Execute);
    }

    #[test]
    fn interrupt_for_past_turn_is_superseded() {
        let mut e = entry("c1", SessionCommandPayload::Interrupt {}, 1_000);
        e.based_on = Some(CommandBasedOn {
            turn_id: Some("turn-1".into()),
            frontier: None,
        });
        let entries = vec![e.clone()];
        let past = |id: &str| id == "turn-1";
        let cx1 = cx(&entries, &NEVER, &past, 2_000, Some("turn-2"));
        assert_eq!(evaluate_command(&e, &cx1), CommandDisposition::Superseded);
        // …but if that turn is still the current one, execute.
        let cx2 = cx(&entries, &NEVER, &past, 2_000, Some("turn-1"));
        assert_eq!(evaluate_command(&e, &cx2), CommandDisposition::Execute);
    }

    #[test]
    fn runs_are_not_superseded_by_newer_runs() {
        // Two queued runs both execute (in order); supersession applies to steer/interrupt only.
        let r1 = entry(
            "r1",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m1".into(),
            },
            1_000,
        );
        let r2 = entry(
            "r2",
            SessionCommandPayload::Run {
                request: run_request(),
                message_id: "m2".into(),
            },
            2_000,
        );
        let entries = vec![r1.clone(), r2.clone()];
        let cx1 = cx(&entries, &NEVER, &NEVER, 3_000, None);
        assert_eq!(evaluate_command(&r1, &cx1), CommandDisposition::Execute);
        assert_eq!(evaluate_command(&r2, &cx1), CommandDisposition::Execute);
    }

    /// The rule that makes "after the turn" a different thing from "this turn"
    /// (gh#424): a follow-up waits, and waiting must not consume it.
    #[test]
    fn a_follow_up_defers_until_it_is_the_head_of_an_unpaused_plan() {
        let entries = vec![queued("q1", 1_000), queued("q2", 2_000)];
        let plan = crate::queue::project(&entries);
        fn base<'a>(
            entries: &'a [SessionCommandEntry],
            running: bool,
            plan: &'a QueueView,
        ) -> EvaluationContext<'a> {
            EvaluationContext {
                is_processed: &NEVER,
                now_ms: 3_000,
                entries,
                current_turn_id: None,
                turn_is_past: &NEVER,
                queue: plan,
                chat_is_running: running,
            }
        }
        // A live turn holds the whole plan — including its head.
        assert_eq!(
            evaluate_command(&entries[0], &base(&entries, true, &plan)),
            CommandDisposition::Defer
        );
        // Idle: the head runs, the rest keep waiting. One turn per settle.
        assert_eq!(
            evaluate_command(&entries[0], &base(&entries, false, &plan)),
            CommandDisposition::Execute
        );
        assert_eq!(
            evaluate_command(&entries[1], &base(&entries, false, &plan)),
            CommandDisposition::Defer
        );

        // Paused holds it too, whatever the position.
        let paused = QueueView {
            paused: Some(crate::queue::QueuePause::User),
            ..plan.clone()
        };
        assert_eq!(
            evaluate_command(&entries[0], &base(&entries, false, &paused)),
            CommandDisposition::Defer
        );

        // Removed from the plan → the host cancels it, and the ledger says so.
        let emptied = QueueView::default();
        assert_eq!(
            evaluate_command(&entries[0], &base(&entries, false, &emptied)),
            CommandDisposition::Cancelled
        );
    }

    /// A follow-up outlives the turn it is waiting on — hours of it, if that is
    /// how long the turn takes. The TTL that protects a stale *send* would here
    /// delete the user's plan for the crime of the agent being slow.
    #[test]
    fn queued_follow_ups_and_their_edits_never_expire() {
        let q = queued("q1", 0);
        assert_eq!(q.effective_expiry(), None);
        let control = entry(
            "c1",
            SessionCommandPayload::QueueControl {
                op: QueueOp::Edit {
                    target: "q1".into(),
                    prompt: "revised".into(),
                    context: Vec::new(),
                },
            },
            0,
        );
        assert_eq!(control.effective_expiry(), None);
        // Everything else keeps the TTL.
        assert_eq!(
            steer("s1", 0).effective_expiry(),
            Some(COMMAND_DEFAULT_TTL_MS)
        );

        let entries = vec![q.clone()];
        let plan = crate::queue::project(&entries);
        let long_after = EvaluationContext {
            is_processed: &NEVER,
            now_ms: COMMAND_DEFAULT_TTL_MS * 1_000,
            entries: &entries,
            current_turn_id: None,
            turn_is_past: &NEVER,
            queue: &plan,
            chat_is_running: false,
        };
        assert_eq!(
            evaluate_command(&q, &long_after),
            CommandDisposition::Execute,
            "a plan queued behind a three-hour turn is still a plan"
        );
    }

    /// Steer supersedes, a follow-up does not. Both carry a prompt; only one of
    /// them is a correction of the other.
    #[test]
    fn a_newer_follow_up_never_supersedes_an_older_one() {
        let entries = vec![queued("q1", 1_000), queued("q2", 2_000)];
        let plan = crate::queue::project(&entries);
        let cx1 = EvaluationContext {
            is_processed: &NEVER,
            now_ms: 3_000,
            entries: &entries,
            current_turn_id: None,
            turn_is_past: &NEVER,
            queue: &plan,
            chat_is_running: false,
        };
        assert_eq!(evaluate_command(&entries[0], &cx1), CommandDisposition::Execute);
        assert_eq!(evaluate_command(&entries[1], &cx1), CommandDisposition::Defer);
        // …whereas two steers leave only the newest.
        let steers = vec![steer("s1", 1_000), steer("s2", 2_000)];
        let cx2 = cx(&steers, &NEVER, &NEVER, 3_000, None);
        assert_eq!(
            evaluate_command(&steers[0], &cx2),
            CommandDisposition::Superseded
        );
    }

    /// The wire shape older engines and the TS edge see. Additive: an entry
    /// written before gh#424 reads back with no references, and a queue entry
    /// round-trips through the same tagged form every other command uses.
    #[test]
    fn the_new_kinds_round_trip_and_the_old_ones_are_unchanged() {
        let mut q = queued("q1", 7);
        q.context = vec![
            comet_proto::ContextRef::file("src/main.rs"),
            comet_proto::ContextRef::directory("crates/ui"),
        ];
        let json = serde_json::to_value(&q).expect("serialize");
        assert_eq!(json["payload"]["kind"], "queue");
        assert_eq!(json["context"][0]["path"], "src/main.rs");
        assert_eq!(json["context"][1]["kind"], "directory");
        let back: SessionCommandEntry = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, q);

        // A pre-gh#424 entry: no `context` key at all.
        let legacy = serde_json::json!({
            "id": "c1",
            "payload": {"kind": "steer", "prompt": "go", "messageId": null},
            "issuedBy": "device-a",
            "issuedAt": 1,
            "status": "pending",
        });
        let back: SessionCommandEntry = serde_json::from_value(legacy).expect("legacy entry");
        assert!(back.context.is_empty());
        assert_eq!(back.kind(), SessionCommandKind::Steer);

        let control = entry(
            "c2",
            SessionCommandPayload::QueueControl {
                op: QueueOp::Move {
                    target: "q1".into(),
                    after: None,
                },
            },
            9,
        );
        let json = serde_json::to_value(&control).expect("serialize");
        assert_eq!(json["payload"]["kind"], "queueControl");
        assert_eq!(json["payload"]["op"]["op"], "move");
        let back: SessionCommandEntry = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, control);
    }

    #[test]
    fn composer_cancel_rules() {
        let e = steer("c1", 1_000);
        assert!(can_composer_cancel(&e, "device-a"));
        assert!(!can_composer_cancel(&e, "device-b"));
        let mut applied = e.clone();
        applied.status = SessionCommandStatus::Applied;
        assert!(!can_composer_cancel(&applied, "device-a"));
    }

    fn run_request() -> RunRequest {
        RunRequest {
            prompt: "hello".into(),
            model: None,
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
        }
    }
}
