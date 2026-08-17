//! How a fork is *named* on every surface (gh#425).
//!
//! The wire words live on the types themselves ([`crate::fork`]); these are the
//! display ones, here for the reason the rest of `view` is: desktop and phone
//! must not invent two vocabularies for one distinction. If one calls it
//! "same checkout" and the other merely "shared", a reader has to work out
//! that they are the same thing — and the
//! whole point of this feature is that the two destinations are *not* the same
//! thing and must never blur.

use crate::fork::{ChatLineage, ForkCheckout};

/// The menu-row title for a destination — an imperative noun phrase, because it
/// is a thing you are about to make rather than a property of one.
pub fn destination_title(checkout: ForkCheckout) -> &'static str {
    match checkout {
        ForkCheckout::Shared => "In this checkout",
        ForkCheckout::Isolated => "In a new worktree",
    }
}

/// The tag a compact row shows: two words, always distinguishable at a glance.
pub fn destination_tag(checkout: ForkCheckout) -> &'static str {
    match checkout {
        ForkCheckout::Shared => "shared read-only",
        ForkCheckout::Isolated => "own worktree",
    }
}

/// The one-line lineage a narrow surface has room for:
/// `fork of “Fix the parser” · shared read-only · copy`.
///
/// Shorter than [`ChatLineage::summary`] on purpose — that one is for a strip
/// with a whole line to itself.
pub fn compact(lineage: &ChatLineage, source_title: Option<&str>) -> String {
    let source = match source_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(title) => format!("\u{201c}{title}\u{201d}"),
        None => "a deleted chat".to_string(),
    };
    format!(
        "fork of {source} · {} · {}",
        destination_tag(lineage.checkout),
        match lineage.context {
            crate::fork::ForkContext::NativeResume => "resumed",
            crate::fork::ForkContext::TranscriptCopy => "copy",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork::{ForkContext, TranscriptTruncation};

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureCase {
        checkout: ForkCheckout,
        context: ForkContext,
        checkout_tag: String,
        context_tag: String,
        truncation: Option<TranscriptTruncation>,
        #[serde(default)]
        truncated: bool,
        truncation_text: Option<String>,
    }

    fn lineage(checkout: ForkCheckout, context: ForkContext) -> ChatLineage {
        ChatLineage {
            source_chat_id: "c1".into(),
            source_message_id: None,
            checkout,
            context,
            copy_reason: None,
            carried_messages: None,
            truncated: false,
            truncation: None,
            forked_at: chrono::Utc::now(),
        }
    }

    /// The rule this module exists for: no two surfaces, and no two
    /// destinations, may share a word.
    #[test]
    fn the_destinations_never_share_a_word() {
        assert_ne!(
            destination_title(ForkCheckout::Shared),
            destination_title(ForkCheckout::Isolated)
        );
        assert_ne!(
            destination_tag(ForkCheckout::Shared),
            destination_tag(ForkCheckout::Isolated)
        );
        let shared = compact(
            &lineage(ForkCheckout::Shared, ForkContext::TranscriptCopy),
            Some("Fix"),
        );
        let isolated = compact(
            &lineage(ForkCheckout::Isolated, ForkContext::TranscriptCopy),
            Some("Fix"),
        );
        assert_ne!(shared, isolated);
        assert!(shared.contains("shared read-only"));
        assert!(isolated.contains("own worktree"));
    }

    #[test]
    fn legacy_rows_and_new_copies_remain_visually_distinct() {
        assert!(
            compact(
                &lineage(ForkCheckout::Shared, ForkContext::NativeResume),
                None
            )
            .ends_with("resumed")
        );
        assert!(
            compact(
                &lineage(ForkCheckout::Shared, ForkContext::TranscriptCopy),
                None
            )
            .ends_with("copy")
        );
    }

    #[test]
    fn rust_and_swift_share_the_checked_in_fork_vocabulary_cases() {
        let cases: Vec<FixtureCase> = serde_json::from_str(include_str!(
            "../../../../apps/ios/Comet/Spec/fork-spec.json"
        ))
        .unwrap();
        for case in cases {
            let mut lineage = lineage(case.checkout, case.context);
            lineage.truncation = case.truncation;
            lineage.truncated = case.truncated;
            if lineage.truncated {
                lineage.carried_messages = Some(0);
            }
            let rendered = compact(&lineage, Some("Fixture"));
            assert_eq!(destination_tag(case.checkout), case.checkout_tag);
            assert!(rendered.ends_with(&case.context_tag), "{rendered}");
            let expected = case.truncation_text.unwrap_or_default();
            if expected.is_empty() {
                assert!(!lineage.detail().contains("size budget"));
            } else {
                assert!(lineage.detail().contains(&expected.to_lowercase()));
            }
        }
    }
}
