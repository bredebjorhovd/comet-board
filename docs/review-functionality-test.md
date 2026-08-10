# Review functionality test

This is a disposable, intentionally reviewable change for
[issue #264](https://github.com/bredebjorhovd/comet-board/issues/264). The
associated pull request should be rejected by a human; it must not be merged.

## What this exercises

The board's review loop has a few claims that are easy to verify from one
small pull request:

1. An open pull request is the author's completion signal and leaves the task
   in `review` with the authoring chat intact.
2. Feedback from the issue-comment, inline-comment, and review-submission
   endpoints is actionable. A `changes requested` submission is actionable
   even if its body is empty.
3. On first sight, comments older than the attempt's end are consumed but not
   delivered, so the agent does not receive its own PR-opening chatter.
4. Each endpoint has an independent watermark. Re-polling after the PR's
   `updated_at` changes does not deliver an already-consumed comment again.
5. Delivery is limited to a live chat whose cwd is still inside the attempt's
   worktree.

The executable claim matrix is in
`crates/board/tests/review_functionality_test.rs`. It deliberately models the
human action from the issue rather than introducing a second network-dependent
test harness.

## Manual review script

After the PR is open:

1. Confirm the PR description links back to issue #264.
2. Leave a `Changes requested` review with a short explanation.
3. Add either an inline comment or an issue comment as a second feedback
   endpoint.
4. Confirm the authoring chat receives the composed review message once.
5. Reject/close the PR manually. Do not merge it.

The expected delivery names the PR, includes the review state and comment
locations, and tells the author to address feedback on the same branch instead
of opening a second PR.
