// Which variant of the theme this device is painting (gh#257).
//
// The desktop picks its variant from a settings page and installs it as a
// global (`comet_ui::theme::ThemeChoice`). The stored default here is System,
// ready to follow the trait collection that `themed(dark:light:)` resolves
// against. The still-shipped bundle override described below currently makes
// that default resolve to Dark, however, so only the explicit Light/Dark
// choices can change the shipped app today.
//
// # What replaced `.preferredColorScheme(.dark)`
//
// Four call sites used to force dark: the root scene and three sheets. The
// sheets had to repeat it because a sheet is its own presentation host. Every
// one of those four sites now installs the same appearance adapter. The adapter
// writes one explicit Light/Dark choice at the UIWindow boundary; it does not
// ask SwiftUI and UIKit to race over the same property.
//
// # The one line that is not here (Info.plist)
//
// `Info.plist` still carries `UIUserInterfaceStyle = Dark`, which forces every
// window in the app regardless of what SwiftUI prefers. gh#257 ring-fenced that
// file, so this code reads the key instead of removing it, and tells the truth
// about it in three places: `Appearance.system` resolves to the forced style
// rather than to "whatever iOS says" (because that is what will actually be
// painted), `Appearance.offered` drops System from the picker while the key is
// set, and `WindowScheme` below re-asserts the window override.
//
// That third one is not belt-and-braces, it is the bug. A SwiftUI
// `preferredColorScheme` and the Info.plist key both end up writing
// `overrideUserInterfaceStyle` on the same UIWindow, and **the last writer
// wins**. `WindowScheme` is therefore the only writer for explicit choices: it
// writes after UIKit attaches the window, and again whenever the scene returns.
//
// Delete the key and all three correct themselves with no code change: System
// starts meaning system, the picker offers it, and `WindowScheme` resolves to
// `.unspecified`, which is what a window with no override has anyway.

import SwiftUI

/// The user's theme preference — the phone's `ThemeChoice`, plus the option the
/// desktop does not need.
enum Appearance: String, CaseIterable, Identifiable {
    /// Follow iOS once the ring-fenced bundle override is removed. This is the
    /// stored default, but today it honestly resolves to that forced style.
    case system
    case light
    case dark

    var id: String { rawValue }

    /// Row label for the picker — the desktop's `ThemeChoice::label`, plus one.
    var label: String {
        switch self {
        case .system: return "System"
        case .light: return "Light"
        case .dark: return "Dark"
        }
    }

    /// The style the window adapter should install. `nil` means "do not
    /// override", which is only honest once nothing else is forcing a style —
    /// see the Info.plist note above.
    var colorScheme: ColorScheme? {
        switch self {
        case .system: return Appearance.forcedByBundle
        case .light: return .light
        case .dark: return .dark
        }
    }

    // MARK: Where the preference comes from

    /// `@AppStorage` key. Shared by the modifier and the picker, so the menu
    /// and the window can never disagree about which one is live.
    static let storageKey = "appearance"

    /// The app-level style `Info.plist` forces on every window, if any. A
    /// window-level override still beats it — which is why Light and Dark work
    /// today — but "no override" cannot: it inherits the forced style.
    static var forcedByBundle: ColorScheme? {
        switch Bundle.main.object(forInfoDictionaryKey: "UIUserInterfaceStyle") as? String {
        case "Dark": return .dark
        case "Light": return .light
        default: return nil
        }
    }

    /// The choices worth showing. System is not offered while the bundle forces
    /// a style, because a row that cannot do what it says is worse than a row
    /// that is not there.
    static var offered: [Appearance] {
        forcedByBundle == nil ? allCases : [.light, .dark]
    }

    /// `-theme light|dark|system`, or `COMET_THEME=light|dark` — the desktop's
    /// env override, so a screenshot rig can ask for a variant without touching
    /// the stored preference (`scripts/ios-theme-shots.sh` drives both screens
    /// in both variants this way).
    static var launchOverride: Appearance? {
        let args = ProcessInfo.processInfo.arguments
        if let ix = args.firstIndex(of: "-theme"), ix + 1 < args.count,
           let picked = Appearance(rawValue: args[ix + 1].lowercased()) {
            return picked
        }
        if let env = ProcessInfo.processInfo.environment["COMET_THEME"]?.lowercased() {
            return Appearance(rawValue: env)
        }
        return nil
    }

    /// The live choice: a launch override beats the stored preference, which
    /// beats System.
    static func effective(stored: String) -> Appearance {
        launchOverride ?? Appearance(rawValue: stored) ?? .system
    }
}

/// Apply the explicit preference at the window boundary. The root scene and
/// every sheet use this same adapter.
struct AppearanceModifier: ViewModifier {
    @AppStorage(Appearance.storageKey) private var stored = Appearance.system.rawValue

    func body(content: Content) -> some View {
        let scheme = Appearance.effective(stored: stored).colorScheme
        content
            .background(WindowScheme(scheme: scheme))
    }
}

/// Writes the preference onto the UIWindow itself, and again when the scene
/// comes back — because UIKit writes the Info.plist style onto the same
/// property and whichever of the two goes last is the one you see.
///
/// A zero-size view rather than a scene delegate: it is mounted exactly where
/// the preference is applied, so the two cannot drift apart. When nothing
/// forces a style it resolves to `.unspecified`, which is a window's own
/// default — the workaround costs nothing once it stops being needed.
private struct WindowScheme: UIViewRepresentable {
    let scheme: ColorScheme?

    private var style: UIUserInterfaceStyle {
        switch scheme {
        case .light: return .light
        case .dark: return .dark
        default: return .unspecified
        }
    }

    /// Holds the CURRENT style for the re-activation observer to read, and owns
    /// that observer's lifetime.
    ///
    /// The closure captures `self` **weakly**. A block-based observer is
    /// retained by the notification centre, so a strong capture is a cycle the
    /// coordinator can never break — `deinit` would never run, and every sheet
    /// that was presented and dismissed would leave another live callback
    /// walking every window on every activation. This app presents four of
    /// them.
    /// Once that cycle is broken, `deinit` can remove the observer normally.
    /// The style lives here rather than being captured in the closure for a
    /// second reason: a captured style would re-assert whatever was live when
    /// the view was made, so switching the theme and then backgrounding the app
    /// would switch it back.
    final class Coordinator {
        var style: UIUserInterfaceStyle = .unspecified
        private var observer: NSObjectProtocol?

        func observeActivation() {
            guard observer == nil else { return }
            observer = NotificationCenter.default.addObserver(
                forName: UIScene.didActivateNotification, object: nil, queue: .main
            ) { [weak self] _ in
                guard let self else { return }
                Coordinator.apply(self.style)
            }
        }

        func stopObserving() {
            if let observer {
                NotificationCenter.default.removeObserver(observer)
            }
            observer = nil
        }

        deinit { stopObserving() }

        /// Static so the closure above carries no reference to a view, a
        /// representable or anything else that could outlive its scene.
        static func apply(_ style: UIUserInterfaceStyle) {
            for scene in UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }) {
                for window in scene.windows where window.overrideUserInterfaceStyle != style {
                    window.overrideUserInterfaceStyle = style
                }
            }
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> UIView {
        let view = UIView(frame: .zero)
        view.isUserInteractionEnabled = false
        context.coordinator.observeActivation()
        return view
    }

    func updateUIView(_ view: UIView, context: Context) {
        context.coordinator.style = style
        // After this layout pass, not during it: the window may not be attached
        // yet on the first one, and UIKit's own write lands in between.
        let style = style
        DispatchQueue.main.async { Coordinator.apply(style) }
    }
}

extension View {
    /// The app's scheme preference — never a constant.
    func cometAppearance() -> some View {
        modifier(AppearanceModifier())
    }
}

/// The Appearance rows for the account menu. A `Picker` in a `Menu` renders as
/// an inline checked group, which is the whole control: three words and a tick.
struct AppearanceMenuSection: View {
    @AppStorage(Appearance.storageKey) private var stored = Appearance.system.rawValue

    var body: some View {
        Picker("Appearance", selection: shown) {
            ForEach(Appearance.offered) { choice in
                Text(choice.label).tag(choice.rawValue)
            }
        }
        .pickerStyle(.inline)
    }

    /// The tick has to land on a row that exists: while the bundle forces a
    /// style, a stored "system" IS that style, and the menu says so.
    private var shown: Binding<String> {
        Binding(get: {
            let live = Appearance.effective(stored: stored)
            guard !Appearance.offered.contains(live) else { return live.rawValue }
            return Appearance.forcedByBundle == .light
                ? Appearance.light.rawValue : Appearance.dark.rawValue
        }, set: { stored = $0 })
    }
}
