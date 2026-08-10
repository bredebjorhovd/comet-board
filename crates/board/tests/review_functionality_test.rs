use anyhow::Result;
use comet_board::review::{Decision, Delivered, plan_delivery};
use comet_board::sources::github::{Feedback, FeedbackKind};

fn feedback(
    kind: FeedbackKind,
    id: i64,
    body: &str,
    created_at: &str,
    state: Option<&str>,
) -> Feedback {
    Feedback {
        kind,
        id,
        body: body.into(),
        url: format!("https://github.com/example/repo/pull/264#feedback-{id}"),
        created_at: created_at.into(),
        author: "human-reviewer".into(),
        path: (kind == FeedbackKind::Inline).then(|| "src/review.rs".into()),
        line: (kind == FeedbackKind::Inline).then_some(42),
        state: state.map(str::to_owned),
    }
}

#[test]
fn manual_rejection_delivers_each_feedback_endpoint_once() -> Result<()> {
    let mut delivered = Delivered {
        updated_at: "2026-07-28T11:00:00Z".into(),
        ..Default::default()
    };
    let feedback = vec![
        feedback(
            FeedbackKind::Issue,
            41,
            "Please explain the retry behavior.",
            "2026-07-28T11:30:00Z",
            None,
        ),
        feedback(
            FeedbackKind::Inline,
            7,
            "Add a regression assertion for this branch.",
            "2026-07-28T11:31:00Z",
            None,
        ),
        feedback(
            FeedbackKind::Review,
            900,
            "The review contract is not demonstrated yet.",
            "2026-07-28T11:32:00Z",
            Some("changes_requested"),
        ),
    ];

    let first = plan_delivery(
        &mut delivered,
        "2026-07-28T11:32:00Z",
        "2026-07-28T11:01:00Z",
        || Ok::<_, anyhow::Error>(feedback.clone()),
    )?;
    assert_eq!(first, Decision::Deliver(feedback.clone()));
    assert_eq!(delivered.issue, 41);
    assert_eq!(delivered.inline, 7);
    assert_eq!(delivered.review, 900);

    let second = plan_delivery(
        &mut delivered,
        "2026-07-28T11:33:00Z",
        "2026-07-28T11:01:00Z",
        || Ok::<_, anyhow::Error>(feedback),
    )?;
    assert_eq!(second, Decision::NothingNew);
    Ok(())
}

#[test]
fn first_sight_consumes_old_pr_chatter_but_delivers_a_new_rejection() -> Result<()> {
    let mut delivered = Delivered::default();
    let old = feedback(
        FeedbackKind::Issue,
        10,
        "Opened the PR.",
        "2026-07-28T09:30:00Z",
        None,
    );
    let rejection = feedback(
        FeedbackKind::Review,
        900,
        "Not quite.",
        "2026-07-28T11:00:00Z",
        Some("changes_requested"),
    );

    let decision = plan_delivery(
        &mut delivered,
        "2026-07-28T12:00:00Z",
        "2026-07-28T10:00:00Z",
        || Ok::<_, anyhow::Error>(vec![old, rejection.clone()]),
    )?;
    assert_eq!(decision, Decision::Deliver(vec![rejection]));
    assert_eq!(delivered.issue, 10);
    Ok(())
}
