# The phone could unpin an orchestrator and never pin one — **done** (gh#166)

`BoardStore.setOrchestrator(chatId:)` has taken a chat id since §gh#144 wrote it,
written the same `[defaults] orchestrator_chat` key through the same validated
`WriteBoardConfig`, and applied nothing optimistically. Its only caller passed
`nil`. The kill switch shipped; the affirmative half was never given a surface,
so a board pinned from the desktop could be unpinned from the phone and then
only re-pinned by ssh-ing to the box.

- **Saying it does not do it, and the chat cannot do it itself.** Telling a
  session "you are now the orchestrator" hands it the brief and moves no
  address: the pin is one chat id in `routing.toml` that `announce` queues
  prompts into, and the two are separable — only the address was missing on
  iOS. Nor could the chat close the gap from inside: an agent learns its own id
  from `COMET_BOARD_CHAT_ID`, which only a *dispatched* run carries, and an
  orchestrator is by definition a chat somebody opened.
- **Three surfaces, one item, one voice.** `OrchestratorPin.swift` holds the
  words (`Pin as orchestrator` / `Unpin as orchestrator`, the desktop's and the
  TUI's), the confirmation, and both refusals; the surfaces hold only where the
  menu hangs. The chat screen's ⋯ menu — because an idle orchestrator has no
  Active row and, before it is pinned, no slot either — plus a long-press on
  the Active or Needs-you row of a chat that is running, and on the pinned slot,
  where §gh#144's unpin already lived. Same modifier there now: that row is the
  pinned case of the same item, not a second one.
- **Pinning asks; unpinning does not.** The cost of the pin is that chat's
  attention — every settle, block, orphan and cap warning arrives in it as a
  prompt — so the dialog says so, and says the way back is a long-press on the
  ◆ slot it is about to gain. It also names the chat it displaces, because
  `orchestrator_chat` is one key: a second pin *moves* it, and a menu that let
  you read it as "add" would be lying about the config it writes. Unpinning
  stays immediate: a kill switch that asks a question is not one.
- **Not offered on a chat the board dispatched.** That is the one thing
  `comet-board doctor` says can be wrong with a pin — an attempt holds a
  workspace slot and pinning it exempts it from its own time cap. The phone
  declines to offer the mistake rather than reporting it afterwards; the *unpin*
  is never withheld, since whatever is pinned has to be removable here however
  it got pinned.
- **Still nothing optimistic, and the same refusals.** Both halves call the one
  `setOrchestrator`, so "No board host" and a write the box refused come back in
  the words they always did — only the verb in the alert's title follows what
  the operator asked for. The board republishes the pin on the watch stream as
  the write lands, so the slot *appearing* is the box agreeing, exactly as the
  slot disappearing already was.
