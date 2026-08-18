# Legacy board chats recover their push contract — **done** (gh#494)

gh#440 made the board's GitHub permission promise durable, but the first
release treated every older `pushRepo`-only chat as permanent corruption. The
workspace scanned those rows every fifteen seconds and logged each refusal,
while every new run or live steer failed before accepting its prompt.

- **The atomic shadow is authoritative.** When `boardPush` exists and its repo
  matches the replaceable config, reconciliation restores both config fields.
  A different repo or an invalid contract still fails closed.
- **True legacy rows earn a replacement contract.** A repo-only row without a
  shadow is upgraded lazily on its owning host. The current board credential
  must prove its complete handoff and repository-contents write access first.
  The persisted replacement deliberately promises no workflow-file access,
  because the original brief's stronger promise was never stored.
- **Failure never becomes ambient authentication.** Missing credentials,
  helper binaries, or capability evidence leave the legacy row untouched and
  refuse the turn before its prompt is written.
- **Expected legacy state is quiet.** Periodic workspace publication leaves a
  pre-contract row for the host resolver without warning every fifteen
  seconds. Conflicting tuples remain visible errors.
- **Mixed versions converge.** A predecessor config that retains only the
  matching repo is repaired from `boardPush`; typed config writes and materialized
  chat rows receive the complete tuple again.

The migration is intentionally lazy rather than probing every historical repo
at application startup. Opening an old transcript stays local and cheap; the
first action that could hand credentials to an agent performs the proof and
persists its result for every later turn and device.
