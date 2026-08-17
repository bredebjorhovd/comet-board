//! Forking a conversation: the wire contract, and the lineage a fork carries
//! forever (gh#425).
//!
//! A comet chat has one linear history. Trying a second approach, or asking a
//! stronger model to review the first, meant opening a new chat and carrying
//! the context across by hand — or steering the original and losing the clean
//! branch point. A fork makes any completed message a branch point with two
//! destinations that must never be mistaken for each other:
//!
//! - [`ForkCheckout::Shared`] — another conversation over the *same working
//!   directory*. Nothing is copied on disk, so a reviewer here watches the
//!   original agent's edits appear underneath it while its own execution stays
//!   explicitly read-only.
//! - [`ForkCheckout::Isolated`] — a new worktree on a branch of its own. Two
//!   agents, two trees, one repo; the fork's mistakes are its own.
//!
//! The other half is what the destination actually *knows*. Every new fork
//! carries a bounded copy of the visible transcript and starts an independent
//! provider session; a resume id owns one mutable conversation and is not a
//! clone primitive. [`ForkContext::NativeResume`] remains decode-only for old
//! synced rows. Naming what happened is the point — copied text that reads like
//! a conversation is not the original provider conversation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HarnessId;

/// Where a fork's agent works, relative to the chat it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ForkCheckout {
    /// The source chat's own working directory. No checkout is created and
    /// none is reclaimed; the two conversations share one uncommitted tree.
    Shared,
    /// A fresh worktree on its own branch, cut from where the source chat
    /// stands right now.
    Isolated,
}

impl ForkCheckout {
    /// Two words for a row/tab tag.
    pub fn label(self) -> &'static str {
        match self {
            ForkCheckout::Shared => "shared checkout",
            ForkCheckout::Isolated => "isolated checkout",
        }
    }

    /// What choosing this means, in one sentence — the line a picker shows
    /// under the option and a lineage strip shows on hover. Written so the two
    /// can never read as the same offer.
    pub fn explanation(self) -> &'static str {
        match self {
            ForkCheckout::Shared => {
                "Same folder as the source chat, in read-only review mode: this agent sees \
                 edits as they land without writing into the shared tree."
            }
            ForkCheckout::Isolated => {
                "A new worktree on a branch of its own, cut from where the source chat \
                 stands: nothing this agent does can reach the source's files."
            }
        }
    }
}

/// What a fork was actually given of the conversation behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ForkContext {
    /// Legacy decode-only vocabulary from builds that proposed provider
    /// resume. New forks never construct this value: a provider session is a
    /// mutable conversation, not a clonable snapshot.
    NativeResume,
    /// The visible transcript was copied into the first prompt. What the
    /// provider kept to itself did not come with it, and nothing here pretends
    /// otherwise.
    TranscriptCopy,
}

impl ForkContext {
    /// The tag a row renders.
    pub fn label(self) -> &'static str {
        match self {
            ForkContext::NativeResume => "native resume",
            ForkContext::TranscriptCopy => "visible transcript copy",
        }
    }

    /// The honest sentence. Long on the copy side on purpose: that is the case
    /// where a reader is most likely to assume more was carried than was.
    pub fn explanation(self) -> &'static str {
        match self {
            ForkContext::NativeResume => {
                "The provider resumed its own session, so this agent holds everything \
                 the original did — including what the transcript never showed."
            }
            ForkContext::TranscriptCopy => {
                "The visible transcript was copied into this agent's first prompt. \
                 Hidden provider state — reasoning, cached context, tool inputs the \
                 transcript strips — did not come with it."
            }
        }
    }
}

/// Why this independent fork needed a transcript copy (gh#425) — recorded so
/// "copy" is never just a shrug or an implied provider-session clone.
///
/// Every variant is a fact about the fork *as asked for*, checkable against the
/// source chat, which is what makes the [`ForkContext`] beside it auditable
/// rather than a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CopyReason {
    /// Comet deliberately does not attach two chats to one mutable provider
    /// session. Until a harness offers a real clone operation, even a tail
    /// fork carries an honest transcript copy.
    ProviderCloneUnavailable,
    /// The fork point is an older message; a provider session can only be
    /// picked up where it was left.
    NotTheTail,
    /// The destination runs a different harness than the session belongs to.
    DifferentHarness,
    /// The destination works in a different directory — provider session
    /// stores are keyed by project directory, so a session created elsewhere
    /// is not resumable here. Every isolated fork lands on this.
    DifferentCheckout,
    /// The source chat has no provider session to resume: nothing has run in
    /// it yet, or its session id was tombstoned after a rejected resume.
    NoProviderSession,
}

/// What the handoff byte budget omitted. This is typed rather than inferred
/// from `carried_messages`: an oversized newest message can be partially
/// present while counting as zero fully carried messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TranscriptTruncation {
    OldestMessagesDropped,
    NewestMessageTruncated,
    OldestDroppedAndNewestTruncated,
}

impl TranscriptTruncation {
    pub fn label(self) -> &'static str {
        match self {
            Self::OldestMessagesDropped => "oldest messages dropped at the size budget",
            Self::NewestMessageTruncated => "newest message truncated at the size budget",
            Self::OldestDroppedAndNewestTruncated => {
                "oldest messages dropped and newest message truncated at the size budget"
            }
        }
    }
}

impl CopyReason {
    /// The clause a result sentence appends after "copied because…".
    pub fn label(self) -> &'static str {
        match self {
            CopyReason::ProviderCloneUnavailable => {
                "the provider cannot clone a session into an independent conversation"
            }
            CopyReason::NotTheTail => "the fork point is not the end of the conversation",
            CopyReason::DifferentHarness => "the destination runs a different harness",
            CopyReason::DifferentCheckout => "the destination works in a different directory",
            CopyReason::NoProviderSession => "the source chat has no provider session to resume",
        }
    }
}

/// The four source/destination facts that choose the copy reason shown for one
/// prospective fork. New forks never resume the source provider session.
///
/// Gathered by the engine (which holds all four) and by a frontend previewing a
/// fork before it is made (which can read three of them off the chat row and
/// knows the fourth from where the menu was opened). They go through
/// [`decide_context`] either way, because a preview that disagrees with the
/// result is worse than no preview: it is a promise about memory that the fork
/// then breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkPoint<'a> {
    /// The fork point is the last message in the source chat.
    pub at_tail: bool,
    /// The destination runs the same harness the session belongs to.
    pub same_harness: bool,
    /// The destination works in the directory that session was created under.
    /// False for every isolated fork, by construction.
    pub same_checkout: bool,
    /// Whether the source has a live provider session. Its id is used only to
    /// distinguish "nothing has run" from "the provider cannot clone this
    /// session"; it is never installed on the destination.
    pub session_id: Option<&'a str>,
}

/// Copy and why — the single implementation, shared by the host that performs
/// a fork and by any surface that describes one before it happens.
///
/// Order matters only in that the FIRST true reason is the one reported, and
/// the order chosen is the one a person would give: where you forked from,
/// then what you forked onto, then where it runs, then whether there was
/// anything to resume at all.
pub fn decide_context(point: ForkPoint<'_>) -> (ForkContext, Option<CopyReason>) {
    let reason = if !point.at_tail {
        Some(CopyReason::NotTheTail)
    } else if !point.same_harness {
        Some(CopyReason::DifferentHarness)
    } else if !point.same_checkout {
        Some(CopyReason::DifferentCheckout)
    } else if point
        .session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        Some(CopyReason::NoProviderSession)
    } else {
        Some(CopyReason::ProviderCloneUnavailable)
    };
    (
        ForkContext::TranscriptCopy,
        Some(reason.expect("every independent fork has a copy reason")),
    )
}

/// Where a chat came from, when it came from another one. Lives on the chat row
/// in the workspace doc, so every device — including the ones that cannot fork
/// — can see the relationship without asking the host anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatLineage {
    /// The chat this one was forked from. Kept even when that chat is later
    /// deleted: a dangling source id is still the truthful answer to "where did
    /// this come from", and viewports render the id when they cannot resolve a
    /// title.
    pub source_chat_id: String,
    /// The message the fork was taken at. `None` on forks made from the tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
    pub checkout: ForkCheckout,
    pub context: ForkContext,
    /// Why the transcript was copied. `None` is accepted only for old synced
    /// rows carrying the legacy `NativeResume` vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_reason: Option<CopyReason>,
    /// How many transcript messages were carried into the first prompt.
    /// `None` on legacy rows that predate independent transcript copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_messages: Option<u32>,
    /// The copy hit its byte budget and dropped the oldest messages.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Exact truncation behavior for new rows. `truncated` remains for old
    /// clients and old synced rows; new readers prefer this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TranscriptTruncation>,
    pub forked_at: DateTime<Utc>,
}

impl ChatLineage {
    /// One line for a header strip: `from “Fix the parser” · shared checkout ·
    /// visible transcript copy`. `source_title` is whatever the reader's own
    /// workspace doc has for [`Self::source_chat_id`] — a fork can outlive its
    /// source, so resolving the title is the caller's job and failing to is not
    /// an error.
    pub fn summary(&self, source_title: Option<&str>) -> String {
        let source = match source_title.map(str::trim).filter(|t| !t.is_empty()) {
            Some(title) => format!("“{title}”"),
            None => "a deleted chat".to_string(),
        };
        format!(
            "from {source} · {} · {}",
            self.checkout.label(),
            self.context.label()
        )
    }

    /// The whole story, for a tooltip or a detail sheet: what was shared, what
    /// was carried, and why.
    pub fn detail(&self) -> String {
        let mut out = String::new();
        out.push_str(self.checkout.explanation());
        out.push(' ');
        out.push_str(self.context.explanation());
        if let Some(reason) = self.copy_reason {
            out.push_str(" Copied because ");
            out.push_str(reason.label());
            out.push('.');
        }
        if let Some(count) = self.carried_messages {
            out.push_str(&format!(
                " {count} message{} carried",
                if count == 1 { "" } else { "s" }
            ));
            if let Some(truncation) = self.truncation {
                out.push_str(", ");
                out.push_str(truncation.label());
            } else if self.truncated {
                out.push_str(", older transcript omitted at the size budget");
            }
            out.push('.');
        }
        out
    }
}

/// `ForkChat` params — one call, because the checkout and the chat have to
/// succeed or fail together (an isolated fork that cut a worktree and then
/// failed to make the chat has leaked a branch nobody will ever look for).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkRequest {
    /// The chat being forked.
    pub chat_id: String,
    /// The message to fork at — any completed user or assistant message.
    /// `None` means the tail; it is still an independent transcript copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub checkout: ForkCheckout,
    /// Run the fork on another harness. `None` keeps the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessId>,
    /// Run the fork on another model. `None` keeps the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Destination account slot. `None` deliberately selects the host's
    /// default login; fork creation never silently inherits billing authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Title for the new chat. `None` derives one from the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A first turn to queue in the fork — "review this independently", the
    /// alternative approach to try. `None` creates the fork and starts nothing:
    /// the carried context waits for whatever its first prompt turns out to be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// What a fork became.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkResult {
    pub chat_id: String,
    pub cwd: String,
    /// The branch the fork works on: a freshly cut one for an isolated fork,
    /// the source's for a shared one (`None` when the checkout is not a git
    /// work tree at all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub lineage: ChatLineage,
    /// Whether a first turn was queued.
    #[serde(default)]
    pub prompted: bool,
}

impl ForkResult {
    /// The sentence a frontend shows when the fork lands — the one place the
    /// resume/copy distinction has to survive contact with a toast.
    pub fn note(&self) -> String {
        match self.lineage.context {
            ForkContext::NativeResume => {
                "Forked with the provider session resumed — this agent holds the whole \
                 conversation, hidden state included."
                    .to_string()
            }
            ForkContext::TranscriptCopy => {
                let count = self.lineage.carried_messages.unwrap_or(0);
                let truncated = self
                    .lineage
                    .truncation
                    .map(|kind| format!(", {}", kind.label()))
                    .or_else(|| {
                        self.lineage
                            .truncated
                            .then(|| ", older transcript omitted at the size budget".to_string())
                    })
                    .unwrap_or_default();
                format!(
                    "Forked with {count} message{} of visible transcript{truncated} — \
                     hidden provider state did not come with it.",
                    if count == 1 { "" } else { "s" }
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineage(context: ForkContext) -> ChatLineage {
        ChatLineage {
            source_chat_id: "chat-1".into(),
            source_message_id: None,
            checkout: ForkCheckout::Shared,
            context,
            copy_reason: (context == ForkContext::TranscriptCopy)
                .then_some(CopyReason::DifferentHarness),
            carried_messages: (context == ForkContext::TranscriptCopy).then_some(4),
            truncated: false,
            truncation: None,
            forked_at: Utc::now(),
        }
    }

    /// Old synced rows remain readable but visually distinct from the only
    /// context mode constructed for new forks.
    #[test]
    fn legacy_wire_rows_and_new_copies_never_read_alike() {
        let resumed = ForkResult {
            chat_id: "c".into(),
            cwd: "/tmp".into(),
            branch: None,
            lineage: lineage(ForkContext::NativeResume),
            prompted: false,
        };
        let copied = ForkResult {
            lineage: lineage(ForkContext::TranscriptCopy),
            ..resumed.clone()
        };
        assert_ne!(resumed.note(), copied.note());
        assert!(resumed.note().contains("resumed"));
        assert!(copied.note().contains("4 messages of visible transcript"));
        assert!(copied.note().contains("hidden provider state did not come"));
    }

    /// Shared and isolated are not two words for one thing, on any surface.
    #[test]
    fn the_two_destinations_are_distinct_everywhere() {
        assert_ne!(ForkCheckout::Shared.label(), ForkCheckout::Isolated.label());
        assert_ne!(
            ForkCheckout::Shared.explanation(),
            ForkCheckout::Isolated.explanation()
        );
        let shared = lineage(ForkContext::TranscriptCopy);
        let isolated = ChatLineage {
            checkout: ForkCheckout::Isolated,
            ..shared.clone()
        };
        assert_ne!(shared.summary(Some("Fix")), isolated.summary(Some("Fix")));
    }

    #[test]
    fn a_deleted_source_still_answers_where_this_came_from() {
        let l = lineage(ForkContext::NativeResume);
        assert!(l.summary(None).contains("a deleted chat"));
        assert!(l.summary(Some("  ")).contains("a deleted chat"));
        assert!(l.summary(Some("Fix the parser")).contains("Fix the parser"));
    }

    #[test]
    fn a_copy_says_why_it_was_a_copy() {
        let l = lineage(ForkContext::TranscriptCopy);
        assert!(l.detail().contains("different harness"));
        assert!(l.detail().contains("4 messages carried"));
        assert!(
            !lineage(ForkContext::NativeResume)
                .detail()
                .contains("Copied")
        );
    }

    #[test]
    fn newest_message_truncation_is_visible_in_the_result_sentence() {
        let mut l = lineage(ForkContext::TranscriptCopy);
        l.carried_messages = Some(0);
        l.truncated = true;
        l.truncation = Some(TranscriptTruncation::NewestMessageTruncated);
        let result = ForkResult {
            chat_id: "c".into(),
            cwd: "/tmp".into(),
            branch: None,
            lineage: l,
            prompted: false,
        };
        assert!(result.note().contains("newest message truncated"));
        assert!(!result.note().contains("oldest dropped"));
    }
}
