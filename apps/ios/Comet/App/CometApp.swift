// Comet for iOS — a viewport onto the comet-native mesh. The phone is a peer
// device: it joins the workspace and session doc rooms and drives remote
// engines through the durable command queue.

import SwiftUI

@main
struct CometApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                // The scheme is a preference, not a constant (gh#257): System
                // by default, so the theme's dynamic tokens resolve from iOS.
                .cometAppearance()
                // Monochrome controls: glass buttons, toolbar icons, and
                // toggles render white like the desktop — accent stays paint
                // for status/markdown, never chrome.
                .tint(Theme.text)
                .background(Theme.bg)
                .onChange(of: scenePhase) { _, phase in
                    if phase == .background {
                        model.flushDocs()
                        // Backgrounding is where a phone's "quit" happens: the
                        // system can reclaim the app without warning, and the
                        // draft debounce is a task that dies with it (gh#536).
                        DraftStore.shared.flush()
                    }
                    // Coming back to the app IS the presence poll (gh#145):
                    // rooms no longer beat, so the answer is refreshed when
                    // somebody is there to read it.
                    if phase == .active {
                        model.refreshPresence()
                    }
                }
        }
    }
}

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            switch model.phase {
            case .signedOut:
                SignInView()
            case .pickingOrg(let tokens, let orgs):
                OrgPickerView(tokens: tokens, orgs: orgs)
            case .ready:
                HomeView()
            }
        }
        .task { model.restore() }
    }
}
