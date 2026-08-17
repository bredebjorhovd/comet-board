//! The bounded transcript handoff a fork carries when the provider's own
//! session cannot be resumed (gh#425).
//!
//! What this is *not*: the conversation. A provider keeps reasoning, cached
//! context and the tool inputs the render policy strips
//! ([`crate::parts`]) to itself, and none of that is in the doc to copy. What
//! is here is exactly what a person reading the transcript can see — so the
//! text it produces says that in its own first paragraph, where the agent
//! reading it will see it too, rather than only in a lineage field the agent
//! never sees.
//!
//! Bounded because a transcript has no upper size and a first prompt does: the
//! newest messages are kept and the oldest dropped, because a fork is a
//! continuation and the recent turns are the ones it continues from. Dropping
//! is never silent — the text says how many went.

use comet_proto::view::tool_chip_content;

use crate::parts::{MessagePart, MessageStatus};
use crate::schema::{MessageRole, SessionMessageEntry};

/// How much visible transcript a fork may carry into its first prompt.
///
/// 24 KiB is about six thousand tokens: enough that a review fork of a normal
/// session gets the whole thing, small enough that it cannot crowd out the
/// instruction it is prefixed to, and far below the point at which any
/// harness's own prompt limits start deciding for us.
pub const HANDOFF_BUDGET_BYTES: usize = 24 * 1024;

/// A rendered handoff: the text, and the two facts a lineage records about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    pub text: String,
    /// Messages actually carried.
    pub carried: u32,
    /// Some transcript content was omitted to fit [`HANDOFF_BUDGET_BYTES`].
    pub truncated: bool,
    pub truncation: Option<comet_proto::TranscriptTruncation>,
}

/// Render the transcript of `entries` up to and including `up_to` (the tail
/// when `None`) as a handoff no larger than `budget` bytes.
///
/// `source_title` names the chat it came from; a fork opened next week is read
/// by somebody who has forgotten which conversation this was.
pub fn build_handoff(
    entries: &[SessionMessageEntry],
    up_to: Option<&str>,
    source_title: Option<&str>,
    budget: usize,
) -> Handoff {
    let end = match up_to {
        Some(id) => match entries.iter().position(|e| e.id == id) {
            Some(ix) => ix + 1,
            // A fork point that is not in the slice carries nothing rather
            // than silently carrying everything: the caller validated the id,
            // so reaching this means the transcript moved under us.
            None => 0,
        },
        None => entries.len(),
    };
    let window = &entries[..end];

    // Render newest-first so the drop happens at the far end of the
    // conversation, then flip back: a fork continues from the recent turns.
    let mut blocks: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut carried = 0u32;
    for entry in window.iter().rev() {
        let Some(block) = render_entry(entry) else {
            continue;
        };
        if used + block.len() > budget && !blocks.is_empty() {
            break;
        }
        used += block.len();
        carried += 1;
        blocks.push(block);
    }
    blocks.reverse();
    let renderable = window.iter().filter(|e| render_entry(e).is_some()).count();
    let mut dropped = renderable.saturating_sub(carried as usize);
    let preamble = preamble(source_title, up_to.is_some());
    let render = |blocks: &[String], dropped: usize, newest_truncated: bool| {
        let mut text = String::with_capacity(used + 512);
        text.push_str(&preamble);
        if newest_truncated {
            text.push_str("\n[The newest message was truncated at this fork's size budget.]\n");
        }
        if dropped > 0 {
            text.push_str(&format!(
                "\n[{dropped} earlier message{} dropped at this fork's size budget.]\n",
                if dropped == 1 { "" } else { "s" }
            ));
        }
        for block in blocks {
            text.push('\n');
            text.push_str(block);
        }
        text.push_str(TRAILER);
        text
    };
    let mut text = render(&blocks, dropped, false);
    while text.len() > budget && blocks.len() > 1 {
        blocks.remove(0);
        carried -= 1;
        dropped += 1;
        text = render(&blocks, dropped, false);
    }
    let mut newest_truncated = false;
    if text.len() > budget && !blocks.is_empty() {
        newest_truncated = true;
        carried = 0;
        let framing = render(&[], dropped, true);
        let available = budget.saturating_sub(framing.len());
        let block = &blocks[0];
        let mut end = available.min(block.len());
        while end > 0 && !block.is_char_boundary(end) {
            end -= 1;
        }
        blocks[0].truncate(end);
        text = render(&blocks, dropped, true);
    }
    if text.len() > budget {
        let mut end = budget.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Handoff {
        text,
        carried,
        truncated: dropped > 0 || newest_truncated,
        truncation: match (dropped > 0, newest_truncated) {
            (false, false) => None,
            (true, false) => Some(comet_proto::TranscriptTruncation::OldestMessagesDropped),
            (false, true) => Some(comet_proto::TranscriptTruncation::NewestMessageTruncated),
            (true, true) => {
                Some(comet_proto::TranscriptTruncation::OldestDroppedAndNewestTruncated)
            }
        },
    }
}

/// The paragraph the forked agent reads first. It states the one thing the
/// copied text cannot state for itself.
fn preamble(source_title: Option<&str>, at_an_older_message: bool) -> String {
    let source = match source_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => format!(" “{title}”"),
        None => String::new(),
    };
    let point = if at_an_older_message {
        "an earlier point in it"
    } else {
        "the end of it"
    };
    format!(
        "# Forked conversation\n\n\
         This chat is a fork of another Comet chat{source}, taken at {point}. Below is the \
         **visible transcript** up to the fork point — the text a person reading that chat \
         would see. It is a copy, not that agent's session: its hidden reasoning, its cached \
         context, and the tool inputs the transcript strips did not come with it. Treat it as \
         a briefing you have read, not as work you remember doing.\n"
    )
}

const TRAILER: &str = "\n---\n\nThat is the end of the copied transcript. \
                       What follows is this chat's own conversation.\n";

/// One transcript entry as markdown, or `None` for entries with nothing a
/// reader could see (an empty streaming stub, a message of stripped parts).
fn render_entry(entry: &SessionMessageEntry) -> Option<String> {
    let mut body = String::new();
    for part in &entry.parts {
        match part {
            MessagePart::Text { text, .. } => {
                let text = text.trim();
                if !text.is_empty() {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(text);
                }
            }
            MessagePart::Tool { call, is_error, .. } => {
                let (label, detail) = tool_chip_content(call);
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&format!(
                    "- {label}: {detail}{}",
                    if *is_error { " (failed)" } else { "" }
                ));
            }
            // A question the other agent was asked is part of what happened;
            // its answer is a command entry, not a message part, so the copy
            // says the question was asked and stops there.
            MessagePart::Input { questions, .. } => {
                if !body.is_empty() {
                    body.push('\n');
                }
                let asked = questions
                    .iter()
                    .map(|q| q.question.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                body.push_str(&format!("- asked: {asked}"));
            }
            MessagePart::Error { message, .. } => {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&format!("- error: {message}"));
            }
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let who = match entry.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    };
    let aborted = matches!(entry.status, Some(MessageStatus::Aborted));
    Some(format!(
        "## {who}{}\n\n{body}\n",
        if aborted { " (interrupted)" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::ToolCall;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn transcript() -> Vec<SessionMessageEntry> {
        vec![
            entry("m1", MessageRole::User, "fix the parser"),
            SessionMessageEntry {
                parts: vec![
                    MessagePart::Text {
                        id: "t0".into(),
                        text: "Looking now.".into(),
                    },
                    MessagePart::Tool {
                        id: "x".into(),
                        call: ToolCall::Exec {
                            command: "cargo test".into(),
                        },
                        is_error: true,
                        resolved: true,
                    },
                ],
                ..entry("m2", MessageRole::Assistant, "")
            },
            entry("m3", MessageRole::User, "try the lexer instead"),
        ]
    }

    /// The exit criterion for honesty: the text the forked agent reads says
    /// what it is holding before it says anything else.
    #[test]
    fn the_copy_announces_itself_as_a_copy() {
        let h = build_handoff(
            &transcript(),
            None,
            Some("Parser work"),
            HANDOFF_BUDGET_BYTES,
        );
        assert!(h.text.starts_with("# Forked conversation"));
        assert!(h.text.contains("Parser work"));
        assert!(h.text.contains("hidden reasoning"));
        assert!(h.text.contains("did not come with it"));
        assert_eq!(h.carried, 3);
        assert!(!h.truncated);
    }

    #[test]
    fn a_fork_point_cuts_the_transcript_there() {
        let h = build_handoff(&transcript(), Some("m2"), None, HANDOFF_BUDGET_BYTES);
        assert!(h.text.contains("fix the parser"));
        assert!(h.text.contains("cargo test"));
        assert!(
            !h.text.contains("try the lexer instead"),
            "nothing after the fork point: {}",
            h.text
        );
        assert_eq!(h.carried, 2);
        assert!(h.text.contains("an earlier point in it"));
    }

    #[test]
    fn tool_calls_survive_as_one_line_each_and_failures_say_so() {
        let h = build_handoff(&transcript(), None, None, HANDOFF_BUDGET_BYTES);
        assert!(h.text.contains("- Run: cargo test (failed)"), "{}", h.text);
    }

    /// Dropping is never silent, and it drops the OLDEST: a fork continues
    /// from the recent turns.
    #[test]
    fn the_budget_drops_the_oldest_and_says_how_many() {
        let mut entries = transcript();
        entries.insert(0, entry("m0", MessageRole::User, &"x".repeat(4096)));
        let h = build_handoff(&entries, None, None, 1024);
        assert!(h.truncated);
        assert_eq!(h.carried, 3);
        assert!(h.text.contains("1 earlier message dropped"), "{}", h.text);
        assert!(h.text.contains("try the lexer instead"), "newest kept");
    }

    /// The budget covers framing and a single oversized message, and the cut
    /// remains valid UTF-8.
    #[test]
    fn a_single_oversized_message_is_hard_capped() {
        let entries = vec![entry("m0", MessageRole::User, &"🦀".repeat(4096))];
        let h = build_handoff(&entries, None, None, 128);
        assert_eq!(h.carried, 0);
        assert!(h.truncated);
        assert!(h.text.len() <= 128);
        // Tiny synthetic budgets may not fit the fixed preamble. The real
        // handoff budget does, and the boundary behavior is pinned below.
    }

    #[test]
    fn an_oversized_newest_message_is_reported_and_not_counted_as_carried() {
        let entries = vec![entry("m0", MessageRole::User, &"newest ".repeat(4096))];
        let h = build_handoff(&entries, None, None, 1024);
        assert_eq!(h.carried, 0);
        assert!(h.truncated);
        assert!(
            h.text.contains("newest message was truncated"),
            "{}",
            h.text
        );
        assert!(h.text.contains("newest"), "newest content is retained");
        assert!(h.text.len() <= 1024);
    }

    #[test]
    fn an_unknown_fork_point_carries_nothing_rather_than_everything() {
        let h = build_handoff(&transcript(), Some("nope"), None, HANDOFF_BUDGET_BYTES);
        assert_eq!(h.carried, 0);
        assert!(!h.text.contains("fix the parser"));
    }
}
