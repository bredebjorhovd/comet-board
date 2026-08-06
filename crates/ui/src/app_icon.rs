//! The product icon on the *running* process: the Dock / Cmd-Tab application
//! icon, and a menu-bar status item carrying the same mark. macOS only —
//! [`init`] no-ops elsewhere.
//!
//! Why at runtime and not just in the package: the shipped `Comet.app` takes
//! its Dock icon from `CFBundleIconFile` (dist/macos/Info.plist → comet.icns),
//! but a bare binary — `cargo run -p comet`, any dev launch, anything started
//! outside the bundle — has no Info.plist, so AppKit falls back to the generic
//! executable icon in the Dock and in Cmd-Tab. An unbundled process can still
//! override that, because gpui puts us in `NSApplicationActivationPolicyRegular`
//! at startup (gpui_macos/src/platform.rs) and a regular app owns its Dock
//! tile: `-[NSApplication setApplicationIconImage:]` replaces it. We set it
//! unconditionally — bundled runs get the identical art, so there is nothing
//! worth gating on.
//!
//! The menu-bar presence is a plain `NSStatusItem` whose button carries a
//! *template* image of the comet mark (AppKit then paints the alpha in the menu
//! bar's own style, so it tracks light/dark and the highlighted state), plus a
//! deliberately minimal menu: Show and Quit. Not a tray UI.
//!
//! gpui wraps neither at the pinned rev — its only icon hook is
//! `WindowOptions::icon`, which is X11-only, and it has no status-bar API — so
//! this talks to AppKit directly through the same objc2 crates gpui_macos uses.

use gpui::App;

/// The app icon, 1024×1024. The same file `scripts/package-macos.sh` cuts
/// `comet.icns` from and `scripts/package-linux.sh` installs into the XDG icon
/// theme, so the running process and the packaged one wear one mark.
#[cfg(target_os = "macos")]
static APP_ICON_PNG: &[u8] = include_bytes!("../../../dist/comet.png");

/// Height of the status item's glyph, in points — AppKit's convention for an
/// image in the 22pt menu bar.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const STATUS_ITEM_HEIGHT: f32 = 18.;

/// The comet mark's viewBox height (`assets/icons/comet-logo.svg`). The render
/// scale is derived from it so the status-item bitmap lands on exact retina
/// pixels; [`tests::logo_viewbox_height_matches_the_render_constant`] pins it
/// to the asset so a redrawn mark can't silently start resampling.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LOGO_VIEWBOX_HEIGHT: f32 = 940.;

/// Dress the running process: product icon in the Dock and Cmd-Tab, comet mark
/// in the menu bar. `on_show` runs when the status item's "Show" entry is
/// picked (see `run_app`'s `activate_main_window`). No-op off macOS.
pub fn init(cx: &mut App, on_show: impl Fn(&mut App) + 'static) {
    #[cfg(target_os = "macos")]
    mac::init(cx, on_show);
    #[cfg(not(target_os = "macos"))]
    {
        // Linux/Windows take their icon from the desktop entry (dist/comet.desktop
        // → `Icon=comet`, installed into the XDG icon theme by the packaged
        // install.sh) and have no menu bar to sit in.
        let _ = (cx, on_show);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use gpui::{App, AssetSource as _, Global, SMOOTH_SVG_SCALE_FACTOR};
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, Sel};
    use objc2::{
        AnyThread as _, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
    };
    use objc2_app_kit::{
        NSApplication, NSBitmapFormat, NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage, NSMenu,
        NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSData, NSSize, NSString, ns_string};

    use super::{APP_ICON_PNG, LOGO_VIEWBOX_HEIGHT, STATUS_ITEM_HEIGHT};
    use crate::icons;

    /// What the status item's menu asks the app to do. The AppKit action fires
    /// on the main thread from inside menu tracking, where the gpui `App` is
    /// not ours to borrow — so the target only posts, and a foreground task
    /// started by [`init`] applies it with a real `&mut App`. Same channel hop
    /// gpui uses for its own notification delegate
    /// (gpui_macos/src/system_notifications.rs).
    enum Command {
        Show,
        Quit,
    }

    /// Holds the status item alive for the life of the process: `NSStatusBar`
    /// keeps only a weak grip, so dropping the `Retained` makes the item vanish
    /// from the menu bar. The target is retained by its menu items, but is kept
    /// here too so the two can never be separated.
    struct StatusItem {
        _item: Retained<NSStatusItem>,
        _target: Retained<Target>,
    }

    impl Global for StatusItem {}

    pub(super) fn init(cx: &mut App, on_show: impl Fn(&mut App) + 'static) {
        let Some(mtm) = MainThreadMarker::new() else {
            // gpui runs the `App::run` callback on the main thread; if that
            // ever stops holding, every AppKit call below would be unsound.
            tracing::warn!("app icon: not on the main thread — skipping");
            return;
        };
        set_application_icon(mtm);

        let (tx, mut rx) = mpsc::unbounded();
        let Some(status_item) = status_item(cx, mtm, tx) else {
            return;
        };
        cx.set_global(status_item);
        cx.spawn(async move |cx| {
            while let Some(command) = rx.next().await {
                cx.update(|cx| match command {
                    Command::Show => on_show(cx),
                    // The same graceful path as ⌘Q and the Dock's Quit:
                    // `App::quit` runs the platform shutdown that fires the
                    // `on_app_quit` observers (engine drain + snapshot flush).
                    Command::Quit => cx.quit(),
                });
            }
        })
        .detach();
    }

    /// Point `NSApp` at the shipped icon, which is what the Dock tile and the
    /// Cmd-Tab switcher both read.
    fn set_application_icon(mtm: MainThreadMarker) {
        let data = NSData::with_bytes(APP_ICON_PNG);
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            tracing::warn!("app icon: dist/comet.png did not decode as an NSImage");
            return;
        };
        // SAFETY: main thread (`mtm`), and AppKit retains the image itself.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }

    /// Build the menu-bar item: comet mark, click opens a two-entry menu.
    fn status_item(
        cx: &App,
        mtm: MainThreadMarker,
        tx: mpsc::UnboundedSender<Command>,
    ) -> Option<StatusItem> {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let button = item.button(mtm)?;
        match menu_bar_image(cx) {
            Some(image) => button.setImage(Some(&image)),
            // Rendering the mark is the only step here that can fail; a titled
            // item is still a working menu-bar presence.
            None => {
                tracing::warn!(
                    "app icon: comet mark did not render — status item falls back to a title"
                );
                button.setTitle(ns_string!("Comet"));
            }
        }

        let target = Target::new(mtm, tx);
        let menu = NSMenu::new(mtm);
        // No key equivalents: ⌘Q already quits through the app menu, and a
        // second binding here would only shadow it.
        menu.addItem(&menu_item(
            mtm,
            ns_string!("Show Comet"),
            sel!(cometShow:),
            &target,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&menu_item(
            mtm,
            ns_string!("Quit Comet"),
            sel!(cometQuit:),
            &target,
        ));
        // With a menu attached, AppKit opens it on click and handles the
        // highlight — nothing for us to track.
        item.setMenu(Some(&menu));

        Some(StatusItem {
            _item: item,
            _target: target,
        })
    }

    fn menu_item(
        mtm: MainThreadMarker,
        title: &NSString,
        action: Sel,
        target: &Target,
    ) -> Retained<NSMenuItem> {
        // SAFETY: `action` is implemented by `Target`, which is the target we
        // set, and both outlive the menu (the `StatusItem` global holds them).
        unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                title,
                Some(action),
                ns_string!(""),
            );
            item.setTarget(Some(target));
            item
        }
    }

    /// Rasterize the comet mark into a template `NSImage` sized for the menu bar.
    fn menu_bar_image(cx: &App) -> Option<Retained<NSImage>> {
        let svg = icons::Assets.load(icons::COMET_LOGO).ok().flatten()?;
        // `render_single_frame` multiplies the factor it is given by gpui's
        // `SMOOTH_SVG_SCALE_FACTOR`, so ask for exactly twice the point height:
        // a retina-sharp bitmap that AppKit uses without resampling.
        let scale = STATUS_ITEM_HEIGHT * 2. / (LOGO_VIEWBOX_HEIGHT * SMOOTH_SVG_SCALE_FACTOR);
        let rendered = cx
            .svg_renderer()
            .render_single_frame(&svg, scale)
            .inspect_err(
                |err| tracing::warn!(error = %err, "app icon: comet mark failed to render"),
            )
            .ok()?;
        let size = rendered.size(0);
        let (width, height) = (size.width.0 as usize, size.height.0 as usize);
        // gpui hands back premultiplied BGRA; `NSBitmapImageRep`'s default
        // format is premultiplied RGBA, so only the channel order differs.
        let mut rgba = rendered.as_bytes(0)?.to_vec();
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        // SAFETY: null planes asks AppKit to allocate the pixel buffer itself,
        // to the `bytesPerRow` × `pixelsHigh` we specify here.
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                width as isize,
                height as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                NSBitmapFormat::empty(),
                (width * 4) as isize,
                32,
            )
        }?;
        // SAFETY: `bitmapData` is the buffer just allocated above, exactly
        // `height` rows of `width * 4` bytes — the length of `rgba`.
        unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), rep.bitmapData(), rgba.len()) };

        let image = NSImage::new();
        image.addRepresentation(&rep);
        // Points, not pixels: the representation is 2×, so AppKit picks it for
        // a retina menu bar and halves it on a 1× display.
        image.setSize(NSSize::new(
            (STATUS_ITEM_HEIGHT * width as f32 / height as f32) as f64,
            STATUS_ITEM_HEIGHT as f64,
        ));
        // Template: AppKit re-renders the alpha in the menu bar's own colour,
        // so the mark stays legible in light, dark, and while highlighted.
        image.setTemplate(true);
        Some(image)
    }

    struct TargetIvars {
        tx: mpsc::UnboundedSender<Command>,
    }

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements and `Target` does
        // not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "CometStatusItemTarget"]
        #[ivars = TargetIvars]
        struct Target;

        unsafe impl NSObjectProtocol for Target {}

        impl Target {
            #[unsafe(method(cometShow:))]
            fn comet_show(&self, _sender: Option<&NSObject>) {
                self.ivars().tx.unbounded_send(Command::Show).ok();
            }

            #[unsafe(method(cometQuit:))]
            fn comet_quit(&self, _sender: Option<&NSObject>) {
                self.ivars().tx.unbounded_send(Command::Quit).ok();
            }
        }
    );

    impl Target {
        fn new(mtm: MainThreadMarker, tx: mpsc::UnboundedSender<Command>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(TargetIvars { tx });
            unsafe { msg_send![super(this), init] }
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::AssetSource as _;

    use super::*;
    use crate::icons;

    /// The status-item render scale is derived from the mark's viewBox, so a
    /// redraw at another size would quietly leave the menu-bar glyph resampled.
    #[test]
    fn logo_viewbox_height_matches_the_render_constant() {
        let svg = icons::Assets
            .load(icons::COMET_LOGO)
            .expect("comet-logo.svg loads")
            .expect("comet-logo.svg is a known asset");
        let svg = std::str::from_utf8(&svg).expect("svg is utf-8");
        let view_box = svg
            .split_once("viewBox=\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(value, _)| value)
            .expect("comet-logo.svg declares a viewBox");
        let height: f32 = view_box
            .split_whitespace()
            .nth(3)
            .expect("viewBox has four components")
            .parse()
            .expect("viewBox height parses");
        assert_eq!(height, LOGO_VIEWBOX_HEIGHT);
    }

    /// The Dock icon is handed to AppKit as raw bytes, so a truncated or
    /// swapped-out asset would only show up as a generic icon at runtime.
    #[cfg(target_os = "macos")]
    #[test]
    fn app_icon_asset_is_a_png() {
        assert_eq!(
            &APP_ICON_PNG[..8],
            b"\x89PNG\r\n\x1a\n",
            "dist/comet.png must stay a PNG — NSImage decodes it by content"
        );
    }
}
