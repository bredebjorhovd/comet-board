//! The follow-up plan, folded out of the command ledger (gh#424).
//!
//! There is no queue *store*. There is a list of immutable ledger entries and
//! this function, which reads them the same way on every device that holds the
//! doc. Everything the product asks of the queue falls out of that:
//!
//! - **Survives restart and an offline viewer** — the plan is the doc, and the
//!   doc is snapshotted locally and synced through the room. Nothing to rebuild.
//! - **Converges across two open clients** — both fold the same converged Loro
//!   list, so they compute the same rows in the same order without either
//!   client's local ordering mattering.
//! - **Idempotent mutations** — an edit sets a value, a remove is a tombstone, a
//!   move re-anchors. Replaying any of them changes nothing, which is what makes
//!   an optimistic client safe: it appends, the fold is the truth, and a retried
//!   append is not a second edit.
//!
//! The cost is that mutations are entries too, so a heavily reordered queue
//! grows the ledger. That is the same trade the transcript already makes (the
//! doc compacts daily at 8MB), and a plan is a handful of rows a person typed.

use serde::{Deserialize, Serialize};

use comet_proto::ContextRef;

use crate::commands::{
    QueueOp, SessionCommandEntry, SessionCommandKind, SessionCommandPayload, SessionCommandStatus,
};

/// One row of the visible plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRow {
    /// The `Queue` entry's command id — what every op targets.
    pub id: String,
    /// The prompt as it stands now (the latest edit, or the original).
    pub prompt: String,
    /// The references as they stand now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// Client-minted id for the user entry this becomes when it runs.
    pub message_id: String,
    /// Device that queued it.
    pub issued_by: String,
    pub issued_at: i64,
    /// True once an edit has rewritten it — the tray says so, because a row
    /// whose text differs from what somebody remembers typing on another device
    /// should say why.
    pub edited: bool,
}

/// Why the plan is on hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueuePause {
    /// Somebody paused it.
    User,
    /// A Stop paused it. Stopping a turn means *stop* — a queue that
    /// immediately launched the next follow-up would make the stop button the
    /// least predictable control in the app. The tray says which of the two
    /// this is, so resuming is a decision and not a discovery.
    Stopped,
}

/// The plan as it stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueView {
    #[serde(default)]
    pub rows: Vec<QueueRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<QueuePause>,
    /// A row explicitly selected for delivery ahead of ordinary queue order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_next: Option<String>,
}

impl QueueView {
    /// Where `command_id` sits in the plan, or `None` when the plan no longer
    /// contains it.
    pub fn position(&self, command_id: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.id == command_id)
    }

    pub fn head(&self) -> Option<&QueueRow> {
        self.rows.first()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Fold the ledger into the plan.
///
/// Ledger order is the base order; ops are applied in ledger order after it, so
/// the last word on any given row wins. Only entries the host has not already
/// resolved are rows: an `Applied` follow-up is a turn that ran, and a
/// `Cancelled` one is a row somebody removed — neither belongs in a list of what
/// is still going to happen.
pub fn project(entries: &[SessionCommandEntry]) -> QueueView {
    let mut rows: Vec<QueueRow> = Vec::new();
    let mut paused: Option<QueuePause> = None;
    let mut run_next: Option<String> = None;

    for entry in entries {
        match &entry.payload {
            SessionCommandPayload::Queue {
                prompt,
                message_id,
                attachments,
            } => {
                if entry.status != SessionCommandStatus::Pending {
                    continue;
                }
                rows.push(QueueRow {
                    id: entry.id.clone(),
                    prompt: prompt.clone(),
                    context: entry.context.clone(),
                    attachments: attachments.clone(),
                    message_id: message_id.clone(),
                    issued_by: entry.issued_by.clone(),
                    issued_at: entry.issued_at,
                    edited: false,
                });
            }
            // A Stop is the user saying stop. It holds the plan (see
            // [`QueuePause::Stopped`]) — and only while there is a plan to hold,
            // so the ordinary "stop this turn, carry on typing" never leaves a
            // paused flag behind for a queue nobody has used.
            SessionCommandPayload::Interrupt {} => {
                if !rows.is_empty() && paused.is_none() {
                    paused = Some(QueuePause::Stopped);
                }
            }
            SessionCommandPayload::QueueControl { op } => match op {
                QueueOp::Edit {
                    target,
                    prompt,
                    context,
                } => {
                    if let Some(row) = rows.iter_mut().find(|row| &row.id == target) {
                        row.prompt = prompt.clone();
                        row.context = context.clone();
                        row.edited = true;
                    }
                }
                QueueOp::Move { target, after } => move_row(&mut rows, target, after.as_deref()),
                QueueOp::Remove { target } => rows.retain(|row| &row.id != target),
                QueueOp::Clear {} => rows.clear(),
                QueueOp::Pause {} => paused = Some(QueuePause::User),
                QueueOp::Resume {} => paused = None,
                QueueOp::RunNext { target } => run_next = Some(target.clone()),
            },
            _ => {}
        }
    }

    run_next = run_next.filter(|target| rows.iter().any(|row| &row.id == target));
    QueueView {
        rows,
        paused,
        run_next,
    }
}

/// Lift `target` out and re-insert it after `after` (head when `None`).
///
/// A missing anchor sends it to the tail rather than dropping the move: the
/// anchor may have been removed by another client between the two clients'
/// views, and "somewhere sensible" beats "silently where it was".
fn move_row(rows: &mut Vec<QueueRow>, target: &str, after: Option<&str>) {
    let Some(from) = rows.iter().position(|row| row.id == target) else {
        return;
    };
    if after == Some(target) {
        return; // after itself: a no-op, not a self-splice
    }
    let row = rows.remove(from);
    let to = match after {
        None => 0,
        Some(anchor) => match rows.iter().position(|row| row.id == anchor) {
            Some(ix) => ix + 1,
            None => rows.len(),
        },
    };
    rows.insert(to, row);
}

/// Does this entry belong to the queue plane at all? (The drain uses it to
/// decide whether a pending entry needs the projection computed.)
pub fn is_queue_entry(entry: &SessionCommandEntry) -> bool {
    matches!(
        entry.kind(),
        SessionCommandKind::Queue | SessionCommandKind::QueueControl
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionDoc;
    use crate::commands::SessionCommandPayload;
    use loro::LoroDoc;

    fn entry(id: &str, payload: SessionCommandPayload, at: i64) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload,
            issued_by: "device-a".into(),
            issued_at: at,
            based_on: None,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
            context: Vec::new(),
        }
    }

    fn queued(id: &str, prompt: &str, at: i64) -> SessionCommandEntry {
        entry(
            id,
            SessionCommandPayload::Queue {
                prompt: prompt.into(),
                message_id: format!("m-{id}"),
                attachments: Vec::new(),
            },
            at,
        )
    }

    fn control(id: &str, op: QueueOp, at: i64) -> SessionCommandEntry {
        entry(id, SessionCommandPayload::QueueControl { op }, at)
    }

    fn ids(view: &QueueView) -> Vec<&str> {
        view.rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn the_plan_is_the_queued_entries_in_ledger_order() {
        let log = vec![queued("q1", "first", 1), queued("q2", "second", 2)];
        let view = project(&log);
        assert_eq!(ids(&view), ["q1", "q2"]);
        assert_eq!(view.head().map(|r| r.prompt.as_str()), Some("first"));
        assert_eq!(view.position("q2"), Some(1));
        assert_eq!(view.paused, None);

        // A follow-up the host already ran, or cancelled, is not a plan.
        let mut log = log;
        log[0].status = SessionCommandStatus::Applied;
        assert_eq!(ids(&project(&log)), ["q2"]);
    }

    #[test]
    fn edit_rewrites_in_place_and_the_last_edit_wins() {
        let log = vec![
            queued("q1", "first", 1),
            control(
                "c1",
                QueueOp::Edit {
                    target: "q1".into(),
                    prompt: "revised".into(),
                    context: vec![ContextRef::file("src/main.rs")],
                },
                2,
            ),
            control(
                "c2",
                QueueOp::Edit {
                    target: "q1".into(),
                    prompt: "revised twice".into(),
                    context: Vec::new(),
                },
                3,
            ),
            // An edit naming a row that is gone changes nothing.
            control(
                "c3",
                QueueOp::Edit {
                    target: "ghost".into(),
                    prompt: "nowhere".into(),
                    context: Vec::new(),
                },
                4,
            ),
        ];
        let view = project(&log);
        assert_eq!(ids(&view), ["q1"]);
        assert_eq!(view.rows[0].prompt, "revised twice");
        assert!(view.rows[0].context.is_empty(), "the edit carries the refs");
        assert!(view.rows[0].edited);

        // Idempotence: the same log folded twice is the same plan, and a
        // duplicated op (an optimistic client that retried) changes nothing.
        let mut twice = log.clone();
        twice.push(control(
            "c2-retry",
            QueueOp::Edit {
                target: "q1".into(),
                prompt: "revised twice".into(),
                context: Vec::new(),
            },
            5,
        ));
        assert_eq!(project(&twice), view);
    }

    #[test]
    fn move_re_anchors_and_survives_a_missing_anchor() {
        let base = vec![
            queued("q1", "a", 1),
            queued("q2", "b", 2),
            queued("q3", "c", 3),
        ];

        // To the head.
        let mut log = base.clone();
        log.push(control(
            "c1",
            QueueOp::Move {
                target: "q3".into(),
                after: None,
            },
            4,
        ));
        assert_eq!(ids(&project(&log)), ["q3", "q1", "q2"]);

        // After a neighbour.
        let mut log = base.clone();
        log.push(control(
            "c1",
            QueueOp::Move {
                target: "q1".into(),
                after: Some("q2".into()),
            },
            4,
        ));
        assert_eq!(ids(&project(&log)), ["q2", "q1", "q3"]);

        // Anchored to a row another client removed → the tail, not a drop.
        let mut log = base.clone();
        log.push(control(
            "c1",
            QueueOp::Remove {
                target: "q2".into(),
            },
            4,
        ));
        log.push(control(
            "c2",
            QueueOp::Move {
                target: "q1".into(),
                after: Some("q2".into()),
            },
            5,
        ));
        assert_eq!(ids(&project(&log)), ["q3", "q1"]);

        // Moving a row after itself, and moving a row that is gone: both no-ops.
        let mut log = base.clone();
        log.push(control(
            "c1",
            QueueOp::Move {
                target: "q2".into(),
                after: Some("q2".into()),
            },
            4,
        ));
        log.push(control(
            "c2",
            QueueOp::Move {
                target: "ghost".into(),
                after: None,
            },
            5,
        ));
        assert_eq!(ids(&project(&log)), ["q1", "q2", "q3"]);

        // Replaying the same move is the same plan (idempotent).
        let mut once = base.clone();
        once.push(control(
            "c1",
            QueueOp::Move {
                target: "q3".into(),
                after: None,
            },
            4,
        ));
        let mut twice = once.clone();
        twice.push(control(
            "c1-retry",
            QueueOp::Move {
                target: "q3".into(),
                after: None,
            },
            5,
        ));
        assert_eq!(ids(&project(&twice)), ids(&project(&once)));
    }

    #[test]
    fn remove_and_clear_only_reach_backwards() {
        let log = vec![
            queued("q1", "a", 1),
            queued("q2", "b", 2),
            control(
                "c1",
                QueueOp::Remove {
                    target: "q1".into(),
                },
                3,
            ),
            queued("q3", "c", 4),
            control("c2", QueueOp::Clear {}, 5),
            // Queued AFTER the clear — an earlier clear must not swallow it.
            queued("q4", "d", 6),
        ];
        assert_eq!(ids(&project(&log)), ["q4"]);
    }

    #[test]
    fn pause_is_last_writer_wins_and_a_stop_holds_the_plan() {
        let base = vec![queued("q1", "a", 1)];

        let mut log = base.clone();
        log.push(control("c1", QueueOp::Pause {}, 2));
        assert_eq!(project(&log).paused, Some(QueuePause::User));
        log.push(control("c2", QueueOp::Resume {}, 3));
        assert_eq!(project(&log).paused, None);

        // A Stop with a plan behind it holds the plan…
        let mut log = base.clone();
        log.push(entry("i1", SessionCommandPayload::Interrupt {}, 2));
        assert_eq!(project(&log).paused, Some(QueuePause::Stopped));
        // …and a resume releases it.
        log.push(control("c1", QueueOp::Resume {}, 3));
        assert_eq!(project(&log).paused, None);

        // A Stop with no plan leaves nothing behind — the ordinary stop button
        // must not arm a pause flag for a queue nobody has used.
        let log = vec![entry("i1", SessionCommandPayload::Interrupt {}, 1)];
        assert_eq!(project(&log).paused, None);
        // An explicit pause outranks a later stop's implicit one.
        let mut log = base.clone();
        log.push(control("c1", QueueOp::Pause {}, 2));
        log.push(entry("i1", SessionCommandPayload::Interrupt {}, 3));
        assert_eq!(project(&log).paused, Some(QueuePause::User));
    }

    #[test]
    fn run_next_selects_a_live_row_and_drops_a_removed_target() {
        let mut log = vec![queued("q1", "a", 1), queued("q2", "b", 2)];
        log.push(control(
            "next",
            QueueOp::RunNext {
                target: "q2".into(),
            },
            3,
        ));
        assert_eq!(project(&log).run_next.as_deref(), Some("q2"));

        log.push(control("pause", QueueOp::Pause {}, 4));
        let paused = project(&log);
        assert_eq!(paused.paused, Some(QueuePause::User));
        assert_eq!(paused.run_next.as_deref(), Some("q2"));

        log.push(control(
            "remove",
            QueueOp::Remove {
                target: "q2".into(),
            },
            5,
        ));
        assert_eq!(project(&log).run_next, None);
    }

    #[test]
    fn multiple_followups_and_controls_survive_session_snapshot_reopen() {
        let doc = SessionDoc::init("chat-queue-restart").unwrap();
        let commands = vec![
            queued("q1", "one", 1),
            queued("q2", "two", 2),
            queued("q3", "three", 3),
            control(
                "edit-q2",
                QueueOp::Edit {
                    target: "q2".into(),
                    prompt: "two edited".into(),
                    context: Vec::new(),
                },
                4,
            ),
            control(
                "move-q3",
                QueueOp::Move {
                    target: "q3".into(),
                    after: None,
                },
                5,
            ),
            control("pause", QueueOp::Pause {}, 6),
        ];
        for command in &commands {
            doc.queue_command(command).unwrap();
        }
        let before = project(&doc.read_commands().unwrap());

        let snapshot = doc.export_snapshot().unwrap();
        let reopened = LoroDoc::new();
        reopened.import(&snapshot).unwrap();
        let reopened = SessionDoc::from_doc(reopened);
        let after = project(&reopened.read_commands().unwrap());

        assert_eq!(after, before);
        assert_eq!(
            after
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["q3", "q1", "q2"]
        );
        assert_eq!(after.rows[2].prompt, "two edited");
        assert_eq!(after.paused, Some(QueuePause::User));
    }

    #[test]
    fn two_clients_folding_the_same_log_agree() {
        // The convergence property in the form it actually matters: whatever
        // order the two clients issued their ops in, they fold the doc's
        // converged order — so the assertion is that the fold is a function of
        // the list, and interleavings that Loro resolves the same way here
        // produce the same plan on both sides.
        let laptop_first = vec![
            queued("q1", "a", 1),
            queued("q2", "b", 2),
            control(
                "c-laptop",
                QueueOp::Move {
                    target: "q2".into(),
                    after: None,
                },
                3,
            ),
            control(
                "c-phone",
                QueueOp::Edit {
                    target: "q1".into(),
                    prompt: "a, revised".into(),
                    context: Vec::new(),
                },
                4,
            ),
        ];
        let view = project(&laptop_first);
        assert_eq!(ids(&view), ["q2", "q1"]);
        assert_eq!(view.rows[1].prompt, "a, revised");
        // The same list, folded again by the other client: identical.
        assert_eq!(project(&laptop_first.clone()), view);
    }
}
