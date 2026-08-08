// The spaces list's derivations — the Swift half of `comet_proto::view::spaces`,
// rule for rule.
//
// Two rules live here, both about a list that told the same truth twice.
//
// **One full row per chat (gh#138).** The home screen shows Active and the
// spaces and the sessions on ONE surface, so a working chat used to draw a full
// row in Active and another identical row below it. Active owns a chat while
// its run is live; the sessions list picks it back up when it goes idle, and the
// space row carries a count so the reader can see where the rows went.
//
// **A unique name per row (gh#138).** A repo has one slug and any number of
// checkouts: `~/dev/attn` and the board worktree cut from it are two real
// spaces, and both are called `bredebjorhovd/attn`. Colliding rows take the
// shortest path tail that separates them.
//
// Ported rather than shared for the reason every file in here is: no Rust runs
// on this device. The Rust tests in `crates/proto/src/view/spaces.rs` are the
// specification; these functions must keep answering the way they do.

import Foundation

// MARK: - Unique names within a group

/// A row's name, split at the seam that matters when the row is too narrow for
/// all of it (gh#144): the `base` a reader recognises, and the `qualifier` that
/// tells this row from its twin.
///
/// Two fields and not one string because a row elides from the RIGHT, and the
/// qualifier is the last thing that may be lost — a truncated
/// `bredebjorhovd/attn · board-gh…` reads exactly like the row it was minted to
/// be different from, which is the fix being invisible precisely where it was
/// needed. A renderer with room lays them out with `line`; a renderer without
/// gives the qualifier its own width and shrinks the base instead.
struct SpaceTitle: Hashable {
    /// The name proper: the space's rename, else its folder basename.
    var base: String
    /// The path tail that separates this row from a same-named sibling in its
    /// device group. `nil` when the base already stands alone.
    var qualifier: String?

    /// Both halves on one line, `base · qualifier` — for surfaces with room for
    /// the whole name.
    var line: String {
        guard let qualifier else { return base }
        return "\(base) · \(qualifier)"
    }
}

/// Row titles for one group of spaces: the space's display name, made unique
/// within the group.
///
/// Index-preserving: `out[i]` names `spaces[i]`. Uniqueness is per group, not
/// global, because that is the scope a reader scans — the same repo on the
/// laptop AND on the box is told apart by the device each row already names.
/// Two rows for literally the same path keep the same title: there is no fact
/// left to separate them with, and that IS a duplicate.
func spaceTitles(_ spaces: [Space]) -> [SpaceTitle] {
    let bases = spaces.map(\.displayName)
    var out = bases.map { SpaceTitle(base: $0, qualifier: nil) }
    var done = Set<String>()
    for base in bases where !done.contains(base) {
        done.insert(base)
        let clash = bases.indices.filter { bases[$0] == base }
        guard clash.count > 1 else { continue }
        let deepest = max(1, clash.map { segments(spaces[$0].path).count }.max() ?? 1)
        // The shallowest tail that separates every colliding row — one depth
        // for the whole clash, so the qualifiers line up.
        let depth = (1...deepest).first { depth in
            let tails = clash.map { pathTail(spaces[$0].path, depth) }
            return Set(tails).count == tails.count
        } ?? deepest
        for ix in clash {
            let tail = pathTail(spaces[ix].path, depth)
            if !tail.isEmpty { out[ix].qualifier = tail }
        }
    }
    return out
}

private func segments(_ path: String) -> [String] {
    path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).map(String.init)
}

/// The last `depth` path segments, `/`-joined. Empty for a path with none.
private func pathTail(_ path: String, _ depth: Int) -> String {
    let parts = segments(path)
    return parts.suffix(max(1, depth)).joined(separator: "/")
}

// MARK: - One full row per chat (gh#138)

/// Where every row the Active list draws actually lives: `(chat id, space id)`.
///
/// `AgentRow` carries no space — an attempt is named by its issue, not its
/// folder — so the join happens here, once.
func activePlacements(_ active: [ActiveRow], chats: [Chat]) -> [(chatId: String, spaceId: String?)] {
    active.map { row in
        (row.chatId, chats.first { $0.id == row.chatId }?.spaceId)
    }
}

/// What a space's own list draws, and what it defers to Active.
struct SpaceShelf {
    /// The chats this space draws, in the caller's order: its own, minus every
    /// chat Active is drawing.
    var idle: [String] = []
    /// How many of this space's chats Active holds — the count the space row
    /// carries so the reader can see where the missing rows went.
    var running: Int = 0
}

/// Split a space's chats between the Active list and its own.
///
/// The count comes from the placements rather than from `order` on purpose: an
/// archived chat that is working anyway is in Active (the board half never
/// hides a live run) and out of the ordinary list, and the space row should
/// still admit it exists.
func spaceShelf(spaceId: String, order: [String],
                active: [(chatId: String, spaceId: String?)]) -> SpaceShelf {
    let held = Set(active.map(\.chatId))
    return SpaceShelf(
        idle: order.filter { !held.contains($0) },
        running: active.filter { $0.spaceId == spaceId }.count
    )
}

/// The space row's compact tie to the Active list — `· 3 running` — or nil when
/// the space has nothing up there.
func runningLabel(_ running: Int) -> String? {
    running > 0 ? "· \(running) running" : nil
}

/// What a space's list says INSTEAD of rows when Active holds all of them.
///
/// nil when it has rows to draw, or when it is empty for the ordinary reason
/// (no sessions at all) — that case is the caller's own empty state. A gap
/// where rows should be reads as a bug; this is the truth instead.
func shelfNote(_ shelf: SpaceShelf) -> String? {
    guard shelf.idle.isEmpty, shelf.running > 0 else { return nil }
    return "\(shelf.running) running above · no idle sessions"
}
