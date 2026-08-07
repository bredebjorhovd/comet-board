// Space detail — the phone's answer to the desktop's horizontal session tabs:
// the space's sessions as a vertical list (creation order, like tab order),
// swipe-to-archive (= tab close), and "+" to start a session in this space.

import SwiftUI

struct SpaceView: View {
    @Environment(AppModel.self) private var model
    let spaceId: String
    @Binding var path: [Route]

    private var space: Space? {
        model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        List {
            let chats = model.chats(in: spaceId)
            if chats.isEmpty {
                emptyState
            }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .buttonStyle(PressWashButtonStyle())
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
        }
        .listStyle(.plain)
        .environment(\.defaultMinListRowHeight, 10)
        .scrollContentBackground(.hidden)
        .scrollEdgeEffectStyle(.soft, for: .top)
        .background(Theme.surface.ignoresSafeArea())
        .navigationTitle(space?.displayName ?? "Space")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text(space?.displayName ?? "Space")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    if let space {
                        HStack(spacing: 4) {
                            Image(systemName: "folder")
                                .font(.system(size: 9))
                            Text("\(space.path) · \(model.deviceName(space.deviceId))")
                                .lineLimit(1)
                                .truncationMode(.head)
                        }
                        .font(Theme.sans(10.5))
                        .foregroundStyle(Theme.textMuted.opacity(0.6))
                    }
                }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button {
                    path.append(.newSession(spaceId: spaceId))
                } label: {
                    Image(systemName: "plus")
                }
                .accessibilityLabel("New session")
            }
        }
        .onAppear {
            if model.launchSheet == "newsession" {
                model.launchSheet = nil
                path.append(.newSession(spaceId: spaceId))
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 14) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text("No sessions in this space")
                .font(Theme.sans(13))
                .foregroundStyle(Theme.textFaint)
            Button {
                path.append(.newSession(spaceId: spaceId))
            } label: {
                Text("Start a session")
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .padding(.horizontal, 16)
                    .frame(height: 36)
            }
            .buttonStyle(.glass)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 48)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
    }

    private func shortPath(_ path: String) -> String {
        (path as NSString).lastPathComponent
    }
}

// MARK: - Add a repo (gh#118)

/// The space picker, repo-first.
///
/// It used to open on a folder browser, which asks "which folder on which
/// machine?" — a question about infrastructure, and one a phone is especially
/// badly placed to answer, since it hosts no folders at all. Work lives in
/// GitHub repos and the box is where they run, so this opens on repos: the ones
/// you already have a space for (named by repo, wherever they live) and the ones
/// the board's GitHub App can see that nobody has connected yet.
///
/// Tapping a connected repo opens its space — no host step, because the space
/// knows its device. Tapping an unconnected one runs the gh#97 onboard on the
/// box (clone, space, adopt) with its progress and its refusals right here, and
/// lands you in the result with its issues on the board.
///
/// The folder browser is still one tap away, for the folders that are nobody's
/// repo.
struct NewSpaceSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let onCreated: (String) -> Void

    @State private var query = ""
    @State private var hosts: [BoardStore.RepoHost] = []
    @State private var sweeping = true
    /// Which host a new repo clones onto. One host is not a choice and is not
    /// offered as one; two is a choice nothing here can make for you.
    @State private var target = 0
    @State private var connecting: String?
    @State private var error: String?
    @State private var browsing = false
    @FocusState private var searchFocused: Bool

    private var host: BoardStore.RepoHost? {
        hosts.indices.contains(target) ? hosts[target] : hosts.first
    }

    /// Every host's links and grants, merged. Space ids are unique across
    /// devices; two boards installed on one repo is still one repo.
    private var links: [SpaceSlug] { hosts.flatMap(\.links) }
    private var offers: [RepoOffer] {
        var out: [RepoOffer] = []
        for offer in hosts.flatMap(\.offers)
        where !out.contains(where: { $0.slug.lowercased() == offer.slug.lowercased() }) {
            out.append(offer)
        }
        return out
    }

    private var rows: [RepoRow] {
        filterRepoRows(query, repoRows(spaces: model.spaces, links: links,
                                       offers: offers, host: host?.deviceId))
    }

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 0) {
                searchField
                    .padding(.horizontal, 20)
                    .padding(.top, 4)
                    .padding(.bottom, 12)
                if hosts.count > 1 {
                    hostPicker
                        .padding(.horizontal, 20)
                        .padding(.bottom, 12)
                }
                repoList
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(SheetStyle.panel)
            .navigationTitle("Add a repo")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
            .navigationDestination(isPresented: $browsing) {
                FolderBrowser(onCreated: { id in
                    dismiss()
                    onCreated(id)
                })
            }
        }
        .presentationDetents([.large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
        .task {
            hosts = await model.repoHosts()
            sweeping = false
        }
    }

    // MARK: Pieces

    private var searchField: some View {
        HStack(spacing: 10) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 14, weight: .medium))
                .foregroundStyle(Theme.textFaint)
            TextField("Search repos…", text: $query)
                .font(Theme.sans(15))
                .foregroundStyle(Theme.text)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.go)
                .focused($searchFocused)
                .onSubmit { if let first = rows.first { pick(first) } }
            if sweeping {
                ProgressView()
                    .controlSize(.mini)
                    .tint(Theme.textMuted)
            } else if !query.isEmpty {
                Button {
                    query = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 15))
                        .foregroundStyle(Theme.textFaint)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 14)
        .frame(height: 44)
        .background(whiteAlpha(0.05), in: Capsule())
    }

    /// Two boards in one org is rare and unguessable, so it is the one case the
    /// picker asks about — and only about where a *new* repo lands. Opening a
    /// repo you already have never asks: the space knows its device.
    private var hostPicker: some View {
        VStack(alignment: .leading, spacing: 6) {
            SheetLabel("Clone new repos onto")
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(Array(hosts.enumerated()), id: \.element.deviceId) { ix, candidate in
                        let selected = ix == target
                        Button {
                            UISelectionFeedbackGenerator().selectionChanged()
                            target = ix
                        } label: {
                            HStack(spacing: 7) {
                                Circle()
                                    .fill(model.deviceOnline(candidate.deviceId)
                                        ? Theme.statusCompleted.opacity(0.9) : whiteAlpha(0.18))
                                    .frame(width: 6, height: 6)
                                Text(model.deviceName(candidate.deviceId))
                                    .font(Theme.sans(13, weight: .medium))
                                    .foregroundStyle(selected ? Theme.text : Theme.textMuted)
                            }
                            .padding(.horizontal, 14)
                            .frame(height: 34)
                            .background(selected ? whiteAlpha(0.15) : whiteAlpha(0.05),
                                        in: Capsule())
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
    }

    private var repoList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                if let error {
                    Text(error)
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.danger)
                        .padding(.horizontal, 4)
                }
                let rows = rows
                if rows.isEmpty {
                    emptyState
                } else {
                    SheetCard {
                        ForEach(Array(rows.enumerated()), id: \.element.id) { ix, row in
                            repoRow(row)
                            if ix < rows.count - 1 {
                                SheetSeparator()
                            }
                        }
                    }
                }
                // The other door. A scratch folder that is nobody's repo is a
                // real place to work, and the repo list cannot offer it.
                SheetCard {
                    SheetLinkRow(title: "Browse folders on a device",
                                 systemImage: "folder") {
                        browsing = true
                    }
                }
                if let note = host?.note {
                    // Why the App offered nothing, when it offered nothing. Not
                    // an error: a board on a `GITHUB_TOKEN` has no installations
                    // to enumerate (gh#97), and its spaces are listed regardless.
                    Text(note)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                        .padding(.horizontal, 4)
                } else if !sweeping && hosts.isEmpty {
                    Text("No device in this org is hosting a board, so a new repo has nowhere to be cloned.")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                        .padding(.horizontal, 4)
                }
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 24)
        }
    }

    private func repoRow(_ row: RepoRow) -> some View {
        // The nil check comes first: two nils are equal, and without it every
        // space that is not a GitHub checkout would spin forever.
        let isConnecting = connecting != nil && connecting == row.slug
        return Button {
            pick(row)
        } label: {
            HStack(spacing: 12) {
                Group {
                    if isConnecting {
                        ProgressView()
                            .controlSize(.mini)
                            .tint(Theme.textMuted)
                    } else if row.slug != nil {
                        Image(systemName: "shippingbox")
                            .font(.system(size: 15))
                            // An unconnected repo will make you wait for a
                            // clone; it sits a shade quieter than the rows that
                            // open instantly.
                            .foregroundStyle(row.connected
                                ? Theme.textMuted : Theme.textFaint.opacity(0.7))
                    } else {
                        LineIconView(.folder, size: 16, color: Theme.textMuted)
                    }
                }
                .frame(width: 22)

                VStack(alignment: .leading, spacing: 2) {
                    Text(row.title)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                        .truncationMode(.head)
                    if let subline = subline(row, connecting: isConnecting) {
                        Text(subline)
                            .font(Theme.sans(12))
                            .foregroundStyle(row.connected && !model.deviceOnline(row.deviceId ?? "")
                                ? Theme.warning.opacity(0.8) : Theme.textMuted.opacity(0.7))
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 8)
                Image(systemName: row.connected ? "chevron.right" : "arrow.down.circle")
                    .font(.system(size: row.connected ? 12 : 15, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
        .disabled(connecting != nil)
        .opacity(connecting == nil || isConnecting ? 1 : 0.5)
    }

    /// "@ box · not on the board", or what is happening to it right now.
    private func subline(_ row: RepoRow, connecting: Bool) -> String? {
        if connecting {
            return "Cloning onto \(model.deviceName(host?.deviceId ?? ""))…"
        }
        var parts: [String] = []
        if let device = row.deviceId {
            parts.append(model.deviceOnline(device)
                ? "@ \(model.deviceName(device))"
                : "@ \(model.deviceName(device)) · offline")
        }
        if let note = row.note { parts.append(note) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Image(systemName: "shippingbox")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text(sweeping ? "Looking for your repos…"
                          : (query.isEmpty ? "No repos yet" : "No repos match"))
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
            if !sweeping && query.isEmpty {
                Text("Connect a repo from a board host, or browse a device's folders below.")
                    .font(Theme.sans(13))
                    .foregroundStyle(Theme.textMuted)
                    .multilineTextAlignment(.center)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 40)
    }

    // MARK: Acting

    private func pick(_ row: RepoRow) {
        guard connecting == nil else { return }
        searchFocused = false
        if let spaceId = row.spaceId {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            dismiss()
            onCreated(spaceId)
            return
        }
        guard let slug = row.slug else { return }
        guard let host else {
            error = "No device in this org is hosting a board, so there is nowhere to clone this repo."
            return
        }
        connecting = slug
        error = nil
        Task {
            let result = await model.onboardRepo(slug: slug, host: host.deviceId)
            connecting = nil
            switch result {
            case .connected(let done):
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                dismiss()
                // The space was made on the box and arrives with the next
                // workspace frame; the route resolves it by id either way.
                onCreated(done.spaceId)
            case .failed(let message):
                // These refusals are written to be read: "cannot onboard
                // acme/thing: the board's GitHub App (id 123) cannot see it…".
                error = message
            }
        }
    }
}

// MARK: - The second door: a device's folders

/// The desktop add-space palette's folder half translated to a screen: device
/// tabs, mono breadcrumb with an up button, the device's folders (git repos
/// badged), "Use this folder" pinned at the bottom. Listing comes from the
/// device over the relay (ListFolders); dotfiles are pre-filtered and long
/// listings are truncated at 500 by the engine.
struct FolderBrowser: View {
    @Environment(AppModel.self) private var model
    let onCreated: (String) -> Void

    @State private var deviceId: String?
    @State private var listing: FolderListing?
    @State private var loading = false
    @State private var error: String?
    @State private var creating = false

    private var devices: [DeviceRow] {
        // Engines own folders; this phone can't. Offer every other device.
        (model.demo?.devices ?? model.workspace?.devices ?? [])
            .filter { $0.platform != "ios" }
    }

    private var selectedDeviceId: String? {
        deviceId ?? devices.first?.id
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if devices.isEmpty {
                emptyDevices
            } else {
                deviceTabs
                    .padding(.horizontal, 20)
                    .padding(.top, 6)
                    .padding(.bottom, 14)
                breadcrumbBar
                    .padding(.horizontal, 20)
                    .padding(.bottom, 10)
                folderList
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(SheetStyle.panel)
        .navigationTitle("Folders")
        .navigationBarTitleDisplayMode(.inline)
        .safeAreaInset(edge: .bottom) {
            if !devices.isEmpty {
                useFolderButton
                    .padding(.horizontal, 20)
                    .padding(.top, 8)
                    .padding(.bottom, 6)
                    .background(SheetStyle.panel.opacity(0.94))
            }
        }
        .task(id: selectedDeviceId) {
            await load(path: nil)
        }
    }

    // MARK: Pieces

    private var deviceTabs: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(devices) { device in
                    let selected = device.id == selectedDeviceId
                    Button {
                        guard deviceId != device.id else { return }
                        UISelectionFeedbackGenerator().selectionChanged()
                        deviceId = device.id
                        listing = nil
                    } label: {
                        HStack(spacing: 7) {
                            Circle()
                                .fill(model.deviceOnline(device.id)
                                    ? Theme.statusCompleted.opacity(0.9) : whiteAlpha(0.18))
                                .frame(width: 6, height: 6)
                            Text(device.name)
                                .font(Theme.sans(13, weight: .medium))
                                .foregroundStyle(selected ? Theme.text : Theme.textMuted)
                        }
                        .padding(.horizontal, 14)
                        .frame(height: 36)
                        .background(selected ? whiteAlpha(0.15) : whiteAlpha(0.05), in: Capsule())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private var breadcrumbBar: some View {
        HStack(spacing: 10) {
            Button {
                if let parent = listing?.parent {
                    currentIsRepo = false
                    Task { await load(path: parent) }
                }
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(listing?.parent == nil ? Theme.textFaint.opacity(0.4) : Theme.text)
                    .frame(width: 32, height: 32)
                    .background(whiteAlpha(0.06), in: Circle())
            }
            .buttonStyle(.plain)
            .disabled(listing?.parent == nil)

            Text(listing?.path ?? " ")
                .font(Theme.mono(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.head)
                .frame(maxWidth: .infinity, alignment: .leading)

            if loading {
                ProgressView()
                    .controlSize(.mini)
                    .tint(Theme.textMuted)
            }
        }
    }

    private var folderList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 8) {
                if let error {
                    Text(error)
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.danger)
                        .padding(.horizontal, 4)
                }
                let folders = (listing?.entries ?? []).filter(\.isDir)
                if folders.isEmpty, !loading, error == nil, listing != nil {
                    Text("No folders here")
                        .font(Theme.sans(13))
                        .foregroundStyle(Theme.textFaint)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 28)
                }
                if !folders.isEmpty {
                    SheetCard {
                        ForEach(Array(folders.enumerated()), id: \.element.name) { ix, entry in
                            folderRow(entry)
                            if ix < folders.count - 1 {
                                SheetSeparator()
                            }
                        }
                    }
                }
                if listing?.truncated == true {
                    Text("Listing truncated — this folder has more entries.")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                        .padding(.horizontal, 4)
                }
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 12)
        }
        .opacity(loading && listing == nil ? 0.4 : 1)
    }

    private func folderRow(_ entry: FolderEntry) -> some View {
        Button {
            guard let base = listing?.path else { return }
            let child = base.hasSuffix("/") ? base + entry.name : "\(base)/\(entry.name)"
            currentIsRepo = entry.isRepo
            Task { await load(path: child) }
        } label: {
            HStack(spacing: 12) {
                LineIconView(entry.isRepo ? .folderWithFiles : .folder, size: 16,
                             color: entry.isRepo ? Theme.accent.opacity(0.85) : Theme.textMuted)
                    .frame(width: 22)
                Text(entry.name)
                    .font(Theme.sans(15))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Spacer(minLength: 8)
                if entry.isRepo {
                    Text("git")
                        .font(Theme.mono(10))
                        .foregroundStyle(Theme.accent.opacity(0.85))
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Theme.accent.opacity(0.12), in: Capsule())
                }
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    private var useFolderButton: some View {
        let name = (listing?.path as NSString?)?.lastPathComponent ?? ""
        return SheetPrimaryButton(
            title: creating ? "Creating…" : (name.isEmpty ? "Use this folder" : "Use “\(name)”"),
            enabled: listing != nil && !creating && !loading
        ) {
            create()
        }
    }

    private var emptyDevices: some View {
        VStack(spacing: 12) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text("No devices yet")
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
            Text("Run Comet on a computer first — its folders will show up here.")
                .font(Theme.sans(13))
                .foregroundStyle(Theme.textMuted)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(32)
    }

    // MARK: Data

    private func load(path: String?) async {
        guard let selectedDeviceId else { return }
        loading = true
        error = nil
        let result = await model.listFolders(deviceId: selectedDeviceId, path: path)
        loading = false
        if let result {
            withAnimation(Motion.fadeQuick) {
                listing = result
            }
        } else if listing == nil {
            error = "Couldn't reach \(model.deviceName(selectedDeviceId)). Make sure it's online."
        }
    }

    private func create() {
        guard let selectedDeviceId, let listing else { return }
        creating = true
        // Initial git flag = the isRepo the engine stamped when we descended
        // into this folder; the owning device's SpacesSync re-verifies anyway.
        Task {
            let id = await model.createSpace(deviceId: selectedDeviceId,
                                             path: listing.path, gitDetected: currentIsRepo)
            creating = false
            if let id {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onCreated(id)
            }
        }
    }

    /// isRepo of the folder we're currently inside (set on descend; cleared
    /// when navigating up or switching device — unknowable there).
    @State private var currentIsRepo = false
}
