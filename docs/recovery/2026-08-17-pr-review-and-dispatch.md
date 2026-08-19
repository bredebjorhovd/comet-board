# Recovery manifest: PR Review And Dispatch (gh#482)

Chat `b5b3c796-2f3b-4bb2-8341-95d1d7782927` ("PR Review And Dispatch") diverged
between the Mac and the edge: the Mac held a long local-only transcript suffix
that could no longer be imported into the edge's shallow snapshot. This is the
durable manifest for the operational recovery performed on 2026-08-17, kept in
the repo per the ticket's requirement to document the recovery and its
checksums.

## Failure shape

- The edge/cloud transcript was an exact 249-entry semantic prefix of the Mac
  transcript; the cloud had no cloud-only IDs.
- A differential export from the Mac against the cloud version vector
  succeeded, but importing that delta into a clone of the cloud snapshot
  deterministically failed with `ImportUpdatesThatDependsOnOutdatedVersion`:
  the delta depended on CRDT history the shallow cloud snapshot no longer
  carries. The Mac was the final 0.8.0 artifact and the edge already included
  the gh#450 shallow-history recovery, so this was a data condition, not stale
  code.
- The Mac-only suffix kept growing during diagnosis because the affected turn
  was still streaming (323 → 322 protected pre-restart → 377 after restart →
  380 in the final frozen snapshot). Recovery waited for the turn to settle
  and derived the manifest from the frozen snapshot, not the original count.

## Recovery procedure

Implemented as `comet_sync::build_session_recovery` (crates/sync/src/recovery.rs)
and driven by the `doc_surgery` example (`plan-recovery`, `write-recovery`,
`apply-recovery`), landed with this manifest.

1. Both Comet processes stopped; final frozen snapshots taken of the Mac and
   cloud stores (checksums below). No live SQLite file was edited.
2. The frozen cloud snapshot was used as the compatible CRDT base. The
   recovery refuses to run unless the authoritative transcript is an exact
   prefix of the local transcript and the authoritative command ledger is an
   ordered subsequence of the local ledger, so it cannot silently choose
   between competing histories.
3. The Mac-only semantic suffix was replayed onto that base by stable ID —
   preserving order, timestamps, roles, parts, statuses, continuations,
   attachment references, and provenance — and the reconstructed document was
   verified entry-for-entry and command-for-command against the frozen Mac
   state before install, including a fresh-clone import of the produced
   update against the authoritative version vector.
4. The recovered snapshot was installed into the Mac store offline, then
   published through the normal sync protocol on relaunch.

## What was replayed

| Measure | Authoritative (cloud, frozen) | Local (Mac, frozen) | Replayed |
| --- | --- | --- | --- |
| Transcript entries | 249 | 380 | 131 Mac-only entries |
| Command ledger | 113 | 159 | 46 terminal Mac-only commands |
| Command outcomes | — | — | 12 Pending→terminal transitions |

The planner also reconstructs the exact original command-ledger order; the
durable exact candidate passes SQLite `integrity_check`.

## Verification

- After relaunch, Mac and cloud converged to **383/383 unique ordered message
  IDs with the same transcript hash** (the chat had advanced past 380 once
  sync resumed) — explicit sync acknowledgement observed.
- Independent-client check: the chat was confirmed current on iOS after
  reopening Comet (2026-08-17). No missing entries, no duplicates.
- Post-recovery spot check (2026-08-19): a read-only `sqlite3 .backup` copy of
  the live Mac store inspected with `doc_surgery inspect-chat` showed 508
  transcript entries (the chat has kept advancing since recovery), all stable
  IDs unique, no duplicates.

## Snapshot inventory (quarantined)

Stored under `~/Documents/Comet-Recovery/2026-08-17-pr-review-and-dispatch/`.
SHA-256 of each `docs.sqlite3`:

| Snapshot | Role | SHA-256 |
| --- | --- | --- |
| `docs.sqlite3` (top level) | Protected pre-restart Mac diagnostic copy (322 entries) | `54dee9e646ceaa02682a4d27ebfa1918a9eba4519d6c14cac873376eee98705a` |
| `cloud/` | Cloud store at diagnosis time (249-entry prefix) | `9352e388f70ec91eb7b70d897587fb3aae78af00ee677acb2603e9f703fd407c` |
| `mac-current-after-restart/` | Integrity-checked online backup after restart (377 entries) | `545133a1ec57927d931cf5b1662dd7a2abd5451cae2f3a43b0bcb6f9aefaff84` |
| `mac-final-frozen/` | Final frozen Mac store, recovery source (380 entries, 159 commands) | `5a8e1b4df0d7c0ba055b0fb2de33c54988c2b042086b89659482e0af4658391b` |
| `cloud-final-frozen/` | Final frozen cloud store, recovery base (249 entries, 113 commands) | `5a05d0b0acae9cb02776a6af5c0ba760f5a566fc0b5e23785b65a1bc8ce414c2` |
| `candidate-final/` | Recovered candidate store | `17272c4b4c7126b33564074fcef7122efac9654bf8a9b861c8f6b956a3890ead` |
| `candidate-final-exact/` | Recovered candidate with exact command-ledger order (installed) | `28108ca75d567cb5dbdcac4b895fbe68d4ad9ad2b1357b12b46c22f3fceb0fd3` |

The `mac-current-after-restart` checksum matches the value recorded on the
issue during diagnosis, tying this inventory to the diagnostic evidence.

## Retention

Independent verification has passed, so the ticket's retention condition is
met; the quarantined snapshots are nevertheless retained deliberately as the
only pre-recovery evidence. They are private user data — do not commit them,
sync them, or open them in place (copy first: opening a WAL SQLite file can
mutate `-shm`/`-wal` sidecars).

## Runbook: recovering a shallow-diverged session

With both stores offline (copies, never the live files):

```
cargo run -p comet-sync --example doc_surgery -- \
  plan-recovery  <cloud_data_dir> <local_data_dir> <chat_id>   # dry run, prints manifest
cargo run -p comet-sync --example doc_surgery -- \
  apply-recovery <cloud_data_dir> <local_data_dir> <target_data_dir> <chat_id>
```

`plan-recovery` is read-only. `apply-recovery` refuses to run if the target
store's snapshot no longer byte-matches the frozen local snapshot, and
verifies the installed snapshot by read-back. Publish by relaunching the
client and waiting for sync acknowledgement, then verify the full ID set from
an independent client before releasing any quarantined snapshots.
