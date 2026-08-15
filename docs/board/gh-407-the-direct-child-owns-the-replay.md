# The direct child owns the replay — **done** (gh#407)

Found by the §gh#337 real-stack rig. The changes-requested notice from §gh#289
told every layer above a force-pushed parent to wait because GitHub would replay
its commits. That advice joined two different GitHub events: merging a lower
pull request does replay the branches above it, but force-pushing a lower branch
does not. In the observed stack, the upper branch stayed byte-for-byte at its
old tip and its pull request became `dirty`; waiting could never repair it.

Landed as the direct-child test in [`crate::stacks::Dependents`], actionable
[`crate::review::compose_notice`] and informational
[`crate::review::compose_hold_notice`], selected by
[`crate::review::fan_out_changes`]. The merge path in
[`crate::rebased::rewritten_notice`] is unchanged.

### Force-push and merge have different owners

After a lower layer is force-pushed, one agent has to rebuild the dependent
history. The direct child receives:

    gh stack rebase --upstack

Run from that branch, the command rebases the child and cascades through every
layer above it in stack order. A plain `git rebase origin/<parent>` is not a
substitute: it can replay the parent's old commit as though it belonged to the
child and conflict with the parent's replacement.

After a lower layer merges, GitHub already performs the server-side replay.
That is the event where the child should not race it. §gh#286's rewritten-branch
notice continues to hand the updated checkout to `gh stack sync`, or to `git
fetch && git rebase` when the GitHub stack tool is unavailable.

### One ordered owner, however tall the stack

`Dependents::above` deliberately returns every transitive dependent because all
of them need to leave review and learn that their diffs will move. It does not
follow that every dependent should run the repair.

For a stack `A ← B ← C`, a force-push of `A` wakes both upper layers. `B`
is the direct child, so it alone receives the upstack command and owns the
ordered replay through `C`. `C` receives a hold notice that names `B` as the
owner. It neither rebases onto stale `B` nor races the same stack rewrite.

`Dependents::is_direct_child` makes that topology decision independently of how
the edge reached the board: GitHub's stack object or the board's recorded
`attempts.stacked_on` relation. The per-dependent watermark remains unchanged,
so both the owner and the informed layers are told once per standing review.

### The regression is taller than the original bug

The original focused test had one parent and one child, which could prove the
wording but not ownership. `only_the_direct_child_owns_the_ordered_upstack_rebase`
builds three layers, requests changes on the bottom layer, and proves that both
dependents are watermarked while exactly one delivered prompt contains
`--upstack`. It also proves the top layer is told to wait for the direct child.

The operational conventions, model field, wire field, review semantics, and
board architecture now all describe the same split: a lower force-push needs
one direct-child rebase owner; a lower merge is GitHub's replay. The §gh#289
write-up keeps its original design account and records this later correction
instead of attributing the new implementation to the older ticket.
