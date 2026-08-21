// The degraded state, on screen (gh#527).
//
// On 2026-08-19 the app held 22 rooms on a maxed-out reconnect ladder, logged
// every one of them, and rendered a transcript that looked like a conversation
// nobody had replied to yet. Those two states are indistinguishable to a
// person, and only one of them means "nothing you type will arrive until this
// clears". This strip is the difference, and it is deliberately the same shape
// as the fork strip above it: one line that changes how the rest of the screen
// should be read, expandable to a sentence, never a modal.
//
// It also carries the export, because the incident's other half was that
// nothing could be got OFF the phone — the operator could not screenshot or
// copy from the failing app, and the diagnosis ran on paraphrases over SSH.
// Copy and Share hang off the strip that is already telling you something is
// wrong, which is where a person is standing when they need them.

import SwiftUI

struct SyncDegradedStrip: View {
    @State private var expanded = false
    @State private var shareText: String?

    var body: some View {
        let snapshot = RoomHealth.shared.snapshot
        if snapshot.degraded {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Image(systemName: "antenna.radiowaves.left.and.right.slash")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(Theme.danger)
                    Text(snapshot.headline)
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 6)
                    Menu {
                        Button {
                            UIPasteboard.general.string = RoomHealth.shared.diagnostics()
                        } label: {
                            Label("Copy diagnostics", systemImage: "doc.on.doc")
                        }
                        Button {
                            shareText = RoomHealth.shared.diagnostics()
                        } label: {
                            Label("Share diagnostics", systemImage: "square.and.arrow.up")
                        }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                            .font(.system(size: 12, weight: .medium))
                            .foregroundStyle(Theme.textSubtle)
                    }
                    .accessibilityLabel("Sync diagnostics")
                }
                if expanded {
                    Text(snapshot.detail)
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            .background(Theme.elementActive)
            .contentShape(Rectangle())
            .onTapGesture { expanded.toggle() }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("\(snapshot.headline). \(snapshot.detail)")
            // A sheet rather than an inline ShareLink: the strip is one line
            // tall and the share sheet needs an anchor that outlives a banner
            // which may clear itself the moment a room comes back.
            .sheet(isPresented: Binding(get: { shareText != nil },
                                        set: { if !$0 { shareText = nil } })) {
                if let shareText {
                    ShareSheet(text: shareText)
                }
            }
        }
    }
}

/// UIActivityViewController, because the text being shared is generated at the
/// moment of asking rather than bound to a value the view already holds.
private struct ShareSheet: UIViewControllerRepresentable {
    let text: String

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [text], applicationActivities: nil)
    }

    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}
