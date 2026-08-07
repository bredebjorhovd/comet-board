// The repo-first space picker's list (gh#118) — the Swift half of
// `comet_proto::view::repos`, rule for rule.
//
// The space picker used to ask "which folder on which machine?", which is a
// question about infrastructure — and on a phone it is a question with no good
// answer, since the phone hosts no folders at all. Work lives in GitHub repos
// and the box is where they run, so the picker asks "which repo?" and everything
// else follows: a repo with a space opens it, wherever it lives; a repo without
// one gets cloned onto the box.
//
// The list is a union of two things neither end can compute alone: the spaces
// (from the workspace doc this app already syncs) and the board's App grant
// (from the box, over the relay). Only the device holding a checkout can say
// which repo it is, so the box supplies the `space → slug` links.
//
// Ported rather than shared for the same reason `agentRows` and
// `boardHostCandidates` are: no Rust runs on this device. The Rust tests in
// `crates/proto/src/view/repos.rs` are the specification; these functions must
// keep answering the way they do.

import Foundation

/// One of the host's spaces, resolved to the GitHub repo it is a checkout of.
struct SpaceSlug: Decodable, Hashable {
    var spaceId: String
    var slug: String
}

/// A repo the board's GitHub App can see. The App's grant rather than the
/// operator's repos, deliberately (gh#97): it is exactly the set the box can
/// clone and the sync loop can poll.
struct RepoOffer: Decodable, Hashable {
    var slug: String
    var `private`: Bool
    var archived: Bool
    /// Which half of the board's config it lacks; absent = polled and routed.
    var missing: String?

    var onBoard: Bool { missing == nil }

    private enum CodingKeys: String, CodingKey {
        case slug, `private`, archived, missing
    }
}

/// What one `OnboardRepo` did — the fields the phone needs to land you in the
/// result. The reply carries more (what it wrote, what it would poll); the CLI
/// and the desktop's Settings page are where that gets read.
struct Onboarded: Decodable {
    var slug: String
    var deviceId: String
    var path: String
    var spaceId: String
    var spaceName: String
}

/// One row of the picker: either a space that exists, or a repo with no space
/// yet. `spaceId` is the difference — and also what tapping the row does.
struct RepoRow: Identifiable, Hashable {
    /// The slug when there is one, else the space id. Never collides: a slug
    /// always contains `/` and a space id never does.
    var id: String
    /// What the row is called. A space whose folder is not a GitHub checkout
    /// keeps its own name — inventing an `owner/` for it would be a lie about
    /// where it came from.
    var title: String
    var slug: String?
    /// The space this row opens. nil = the row onboards instead.
    var spaceId: String?
    /// The device that space lives on. The picker never asks for one: a space
    /// knows its own host, and an onboard targets the board's.
    var deviceId: String?
    var isPrivate: Bool
    var archived: Bool
    /// The board polls and routes this repo, so its issues are on the board.
    var onBoard: Bool
    /// The space lives on the board's host — the box.
    var onHost: Bool

    var connected: Bool { spaceId != nil }

    /// The repo-side facts worth a word under the title, if any. Not the device
    /// (the view names that), and nothing at all for the ordinary case: a
    /// connected repo whose issues are on the board is what the picker is *for*.
    var note: String? {
        var parts: [String] = []
        if isPrivate { parts.append("private") }
        if archived { parts.append("archived") }
        if !connected {
            parts.append("not connected yet")
        } else if slug != nil && !onBoard {
            // A space that is a GitHub checkout the board does not watch: its
            // issues are not on the board, which is exactly the surprise this
            // picker exists to stop somebody discovering later.
            parts.append("not on the board")
        }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}

/// The picker's list: every space, plus every offered repo that has none.
///
/// Order is spaces first (the box's before anybody else's, then alphabetical),
/// then the repos with no space. Rows you can open outrank rows that would make
/// you wait for a clone, and among the openable ones the box outranks the
/// laptop, because the box is where work actually runs.
func repoRows(spaces: [Space], links: [SpaceSlug], offers: [RepoOffer],
              host: String?) -> [RepoRow] {
    let slugBySpace = Dictionary(links.map { ($0.spaceId, $0.slug) },
                                 uniquingKeysWith: { first, _ in first })
    func offer(_ slug: String) -> RepoOffer? {
        offers.first { $0.slug.lowercased() == slug.lowercased() }
    }

    var rows: [RepoRow] = spaces.map { space in
        let slug = slugBySpace[space.id]
        let match = slug.flatMap(offer)
        return RepoRow(id: slug ?? space.id,
                       title: slug ?? space.displayName,
                       slug: slug,
                       spaceId: space.id,
                       deviceId: space.deviceId,
                       isPrivate: match?.private ?? false,
                       archived: match?.archived ?? false,
                       onBoard: match?.onBoard ?? false,
                       onHost: host != nil && host == space.deviceId)
    }
    // Two spaces on one repo — a checkout and a clone of it — are two places to
    // work, so both stay. Only the *offer* is deduplicated against them, below.
    rows.sort { a, b in
        if a.onHost != b.onHost { return a.onHost }
        let (ta, tb) = (a.title.lowercased(), b.title.lowercased())
        return ta == tb ? a.id < b.id : ta < tb
    }

    let connected = Set(rows.compactMap { $0.slug?.lowercased() })
    let unconnected = offers
        .filter { !connected.contains($0.slug.lowercased()) }
        .map {
            RepoRow(id: $0.slug, title: $0.slug, slug: $0.slug, spaceId: nil, deviceId: nil,
                    isPrivate: $0.private, archived: $0.archived, onBoard: $0.onBoard,
                    onHost: false)
        }
        .sorted { $0.title.lowercased() < $1.title.lowercased() }
    return rows + unconnected
}

/// Match rank of one row against a query: 0 prefix, 1 substring, nil no match.
/// Case-insensitive; an empty query matches everything at rank 1.
///
/// Both the repo name alone and the whole `owner/repo` count as prefixes:
/// somebody looking for `comet-board` types `comet`, not `bredebjorhovd/comet`,
/// and a picker that made them type the owner first would be the folder browser
/// again with extra steps.
func repoRowRank(_ query: String, _ row: RepoRow) -> Int? {
    let q = query.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    if q.isEmpty { return 1 }
    let title = row.title.lowercased()
    let name = title.split(separator: "/").last.map(String.init) ?? title
    if name.hasPrefix(q) || title.hasPrefix(q) { return 0 }
    return title.contains(q) ? 1 : nil
}

/// Filter + rank the rows for a search query, preserving `repoRows`' order
/// within each rank.
func filterRepoRows(_ query: String, _ rows: [RepoRow]) -> [RepoRow] {
    var ranked: [(rank: Int, ix: Int, row: RepoRow)] = []
    for (ix, row) in rows.enumerated() {
        guard let rank = repoRowRank(query, row) else { continue }
        ranked.append((rank: rank, ix: ix, row: row))
    }
    ranked.sort { a, b in a.rank == b.rank ? a.ix < b.ix : a.rank < b.rank }
    return ranked.map(\.row)
}
