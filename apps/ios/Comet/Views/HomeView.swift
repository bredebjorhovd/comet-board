// Home — the mobile shell. The desktop sidebar's two sections become the
// phone's home screen: Spaces (grouped work) and Sessions (the global
// attention-sorted list). Tabs-as-sessions don't fit a phone; a space opens
// into its own session list instead, and close=archive becomes swipe-to-archive.

import SwiftUI

enum Route: Hashable {
    case space(String)
    case chat(String)
    case newSession(spaceId: String)
    case board
    /// What the board did with the work it was given (gh#143) — a screen off
    /// the board, because it answers a question about the board.
    case stats
}

struct HomeView: View {
    @Environment(AppModel.self) private var model
    @State private var path: [Route] = []
    @State private var showNewSpace = false

    var body: some View {
        NavigationStack(path: $path) {
            List {
                // First, the inbox (gh#122): does anything want me — in
                // words, and it cannot miss. Then the orchestrator's pinned
                // slot, above Spaces, exactly where both desktop sidebars put
                // them.
                NeedsYouSection(path: $path)
                OrchestratorSlotSection(path: $path)
                spacesSection
                // Between Spaces and the sessions, exactly where both desktop
                // sidebars put it: everything alive in one Active group
                // (gh#123) — board attempts (gh#103) and the runs the board
                // never released (gh#117), needs-you first.
                ActiveSection(path: $path)
                sessionsSection
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle("Comet")  // feeds the back menu; not displayed
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(removing: .title)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .space(let id): SpaceView(spaceId: id, path: $path)
                case .chat(let id): SessionView(chatId: id)
                case .newSession(let spaceId): NewSessionView(spaceId: spaceId, path: $path)
                case .board: BoardView(path: $path)
                case .stats: StatsView()
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    // In the bar, not the list: as a list row it appeared and
                    // vanished with the connection and shoved the content down.
                    if !model.connected {
                        ProgressView()
                            .controlSize(.mini)
                            .tint(Theme.textMuted)
                            .accessibilityLabel("Connecting")
                    }
                }
                // Bare spinner — no glass capsule behind it.
                .sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarLeading) {
                    // The board is a place, not a mode: it lives beside the
                    // shell rather than inside a session, and it is reachable
                    // before anything on the list has been opened.
                    Button {
                        path.append(.board)
                    } label: {
                        Image(systemName: "square.stack.3d.up")
                    }
                    .accessibilityLabel("Board")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showNewSpace = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("Add a repo")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        if model.demo != nil {
                            Text("Demo mode")
                        }
                        // The desktop's Appearance settings page, at the size a
                        // phone needs it (gh#257): the theme has two variants
                        // now, and this is the only place that says so.
                        AppearanceMenuSection()
                        Button("Sign out", role: .destructive) { model.signOut() }
                    } label: {
                        Image(systemName: "person.circle")
                    }
                }
            }
            .sheet(isPresented: $showNewSpace) {
                NewSpaceSheet { spaceId in
                    path.append(.space(spaceId))
                }
            }
            .task(id: model.overviewChats.map(\.id).joined()) {
                model.preloadSessions()
            }
            .onAppear {
                if let route = model.launchRoute {
                    model.launchRoute = nil
                    // Push the whole stack atomically — appending from a child's
                    // onAppear mid-transition gets dropped by NavigationStack.
                    if case .space(let id) = route, model.launchSheet == "newsession" {
                        model.launchSheet = nil
                        path = [route, .newSession(spaceId: id)]
                    } else {
                        path = [route]
                    }
                }
                if model.launchSheet == "newspace" {
                    model.launchSheet = nil
                    showNewSpace = true
                }
            }
        }
    }

    // MARK: Spaces

    private var spacesSection: some View {
        Section {
            if model.spaces.isEmpty {
                // gh#118: reaching a space no longer means owning a desktop —
                // "+" offers the board's repos and clones one onto the box.
                Text("No repos yet — tap ＋ to connect one")
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(model.spaces) { space in
                Button {
                    path.append(.space(space.id))
                } label: {
                    SpaceRow(space: space)
                }
                .buttonStyle(SelectRowButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
            }
        } header: {
            sectionHeader("Spaces", count: model.spaces.count)
        }
    }

    // MARK: Sessions

    /// The sessions below Active, minus the ones Active is already drawing
    /// (gh#138): three agents working used to fill this screen twice over, the
    /// same full rows in both lists. A chat lives in exactly one of them —
    /// Active while it runs, here when it is idle.
    private var sessionsSection: some View {
        // Hoisted out of the content builder so the header can count them —
        // "Sessions 3" is a claim about the rows below it (ios.md B5.1).
        let held = Set(model.activeRowPlacements.map(\.chatId))
        let chats = model.overviewChats.filter { !held.contains($0.id) }
        return Section {
            if chats.isEmpty {
                // Say where they went rather than leaving a gap under a header
                // that the list above just proved is busy.
                let shelf = SpaceShelf(idle: chats.map(\.id), running: held.count)
                Text(shelfNote(shelf) ?? "No sessions yet")
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .buttonStyle(SelectRowButtonStyle(cornerRadius: Theme.radiusRow))
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        model.archive(chatId: chat.id)
                    } label: {
                        Label("Archive", systemImage: "archivebox")
                    }
                    .tint(Theme.surfaceRaised)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
        } header: {
            sectionHeader("Sessions", count: chats.count)
        }
    }

    /// A Home section header (ios.md B4.2 / B5.1): the name left, the count
    /// right. Both `--subtle` — on Home a section is a LABEL above rows that
    /// can each be read alone, where on the board the section is the structure
    /// and its header takes `--text`.
    private func sectionHeader(_ title: String, count: Int) -> some View {
        HStack(spacing: 0) {
            Text(title)
                .font(Theme.sans(Theme.textDense, weight: .medium))
                .foregroundStyle(Theme.textSubtle)
            Spacer(minLength: 8)
            Text("\(count)")
                .font(Theme.sans(Theme.textDense))
                .foregroundStyle(Theme.textSubtle)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

// MARK: - Rows

struct SpaceRow: View {
    @Environment(AppModel.self) private var model
    let space: Space

    /// How wide a qualifier may get before it truncates — the sidebar's
    /// `SPACE_QUALIFIER_MAX`, so both surfaces cut the tail at the same place.
    static let qualifierMax: CGFloat = 132

    var body: some View {
        HStack(spacing: 10) {
            // Leading 6pt aggregate dot — position stable, most-urgent member.
            let agg = model.spaceIndicator(space.id)
            // round-ok: a status dot — the space's aggregate
            Circle()
                .fill((agg == .working || agg == .awaitingInput) ? (agg?.dotColor ?? Theme.textFaint) : Theme.textFaint)
                .frame(width: 6, height: 6)
            // ios.md B4.5: the icon says whether this is a repo or a plain
            // folder. The canvas draws the GitHub mark, which needs a remote
            // HOST — and the workspace doc carries `gitDetected` and nothing
            // about where the remote points, so drawing a vendor's mark here
            // would be asserting a fact nobody sent us.
            LineIconView(space.gitDetected ? .gitBranch : .folder,
                         size: 17,
                         color: online ? Theme.textMuted : Theme.textSubtle)
            // Unique within its device's spaces (gh#138): a repo slug names a
            // repo, and one machine can hold a checkout AND a worktree of it.
            //
            // The qualifier is drawn beside the base rather than appended to it
            // (gh#144). This row elides from the right, so one glued string
            // loses the tail FIRST — both checkouts of one repo would read
            // `bredebjorhovd/attn…` again, and the fix would be invisible
            // exactly where it was needed. Beside it, the tail has a width of
            // its own to lose or keep.
            let title = model.spaceTitlesById[space.id]
                ?? SpaceTitle(base: space.displayName, qualifier: nil)
            Text(title.base)
                .font(Theme.sans(Theme.textTitle, weight: .medium))
                .foregroundStyle(online ? Theme.text : Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.tail)
                // Above the qualifier, not level with it: at `textTitle` the
                // two together outrun a 393pt row, and losing the tail of the
                // BASE is what gh#144 was about — two checkouts of one repo
                // reading `comet-na…` again.
                .layoutPriority(2)
            if let qualifier = title.qualifier {
                Text("· \(qualifier)")
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                    .lineLimit(1)
                    // Capped, as the sidebar caps it, and ranked with the base
                    // rather than above it: what the two of them squeeze is the
                    // device tag, which the row can afford to abbreviate.
                    //
                    // A truncated qualifier still separates the rows here — a
                    // phone space is named by its folder, so the tail's LAST
                    // segment repeats the base and the part that differs is at
                    // the front, where truncation never reaches.
                    .frame(maxWidth: SpaceRow.qualifierMax, alignment: .leading)
                    .layoutPriority(1)
            }
            // "· 3 running" — where this space's live rows went. The dot said
            // how urgent; this says how many, and the sessions list below no
            // longer repeats them.
            //
            // Lowest priority of the four, and the only one that may vanish on
            // a narrow row: the leading dot already colours when this space has
            // something live, so the count is the one fact here that is said
            // twice.
            if let running = runningLabel(model.spaceRunning(space.id)) {
                Text(running)
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            // Ranked with the name: a device that has gone offline cannot run
            // this space's sessions at all, and a warning that gets squeezed
            // off the row is a warning nobody sees.
            deviceTag.layoutPriority(1)
            Image(systemName: "chevron.right")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Theme.textFaint)
        }
        .padding(.horizontal, 13)
        .padding(.vertical, 11)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusCard))
    }

    private var online: Bool { model.deviceOnline(space.deviceId) }

    private var deviceTag: some View {
        let name = model.deviceName(space.deviceId)
        // ios.md B4.6: an offline host is `--blocked`, not `--working`. It is
        // not a slow thing, it is a thing that cannot be driven at all.
        return Text(online ? "@ \(name)" : "@ \(name) · offline")
            .font(Theme.sans(Theme.textDense))
            .foregroundStyle(online ? Theme.textSubtle : Theme.status(.blocked))
            .lineLimit(1)
    }
}

/// The desktop session row (shell.rs `render_chat_row`), line for line: the
/// status rail leads a muted context line carrying the space name and the
/// relative time; the title sits on its own line below; harness mark and branch
/// close it out. Lines 2 and 3 indent by rail + gap so they start exactly under
/// the context line rather than beside the rail.
///
/// The one addition the phone needs: the desktop row names only the space
/// because its sidebar sits on the machine running the work. Here the Sessions
/// list interleaves every device, and a session whose host has gone offline
/// can't be driven at all — so the context line reads "space · device".
struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool

    /// Rail (6) + gap (8) — see `render_chat_row`'s `pl(px(14.0))`.
    private static let indent: CGFloat = StatusRail.width + 8

    private var subline: Color { Theme.textSubtle }

    /// Whether line 3 has anything to say (ios.md B5.4). The canvas draws the
    /// third session in its list with no harness and no branch and no third
    /// line at all — a two-line row, its title one tone down, which is what a
    /// chat that never became work looks like.
    private var hasSubline: Bool {
        chat.config?.harness != nil
            || !(chat.branch?.trimmingCharacters(in: .whitespaces) ?? "").isEmpty
    }

    var body: some View {
        let indicator = model.indicator(for: chat)
        VStack(alignment: .leading, spacing: 2) {
            // Line 1: status rail, space · device, time-ago.
            HStack(spacing: 8) {
                StatusRail(indicator: indicator)
                if showLocation {
                    Text(location)
                        .font(Theme.sans(Theme.textDense))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Spacer(minLength: 4)
                }
                Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(subline)
                    .fixedSize()
            }

            // Line 2: the session title.
            // ios.md B5.3 / Deviations: the row title is `textProse`. On a
            // phone the list IS the reading surface, and this is the one line
            // of the row a thumb is aiming at.
            Text(chat.displayTitle)
                .font(Theme.sans(Theme.textProse))
                .foregroundStyle(hasSubline ? Theme.text : Theme.textMuted)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.leading, Self.indent)

            // Line 3: harness brand mark, then the branch when the engine
            // stamped one — and nothing at all when there is neither.
            if hasSubline {
                HStack(spacing: 5) {
                    if let harness = chat.config?.harness {
                        HarnessBadge(harness: harness, size: 11, neutral: subline)
                    }
                    if let branch = chat.branch?.trimmingCharacters(in: .whitespaces), !branch.isEmpty {
                        LineIconView(.gitBranch, size: 11, color: subline)
                        Text(branch)
                            .font(Theme.sans(Theme.textDense))
                            .foregroundStyle(subline)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                    Spacer(minLength: 0)
                }
                .padding(.leading, Self.indent)
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
    }

    /// "space · device", with offline marker. The space name (not the cwd
    /// basename) is what the desktop row shows — they differ once a space has
    /// been renamed, or when the session runs in a worktree off to the side.
    private var location: String {
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        let name = model.deviceName(chat.deviceId)
        return model.deviceOnline(chat.deviceId)
            ? "\(space) · \(name)"
            : "\(space) · \(name) (offline)"
    }
}

func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
