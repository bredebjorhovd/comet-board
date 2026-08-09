//! Monochrome theme — concrete values, no indirection.
//!
//! Colors are precomputed from an oklch-derived neutral scale (perceptually even
//! lightness steps; the same scale comet's Tailwind theme used) into gpui [`Hsla`].
//! **Numbers drive layout, colors are paint**: layout constants live here as plain
//! numbers and never depend on which color is painted.
//!
//! Two variants, [`Theme::dark`] (the default — comet is always-dark by design)
//! and [`Theme::light`] (the same ramp inverted: text goes dark, surfaces light,
//! accents re-tuned for contrast on white). The chosen variant is installed as a
//! gpui [`Global`] at boot (`cx.set_global(Theme::dark())`) and can be flipped
//! live from the Appearance settings page; read with [`Theme::of`]. Hairlines and
//! interactive washes are white at low alpha in dark and black at low alpha in
//! light — see [`Theme::white_alpha`] / [`Theme::wash`].
//!
//! # Four text tones, and no multipliers (gh#172)
//!
//! Text paints in exactly four greys — [`Theme::text`], [`Theme::text_muted`],
//! [`Theme::text_subtle`], [`Theme::text_faint`] — and **a text tone is never
//! multiplied by an alpha at the call site**. Before this rule the three tones
//! that existed were multiplied by eleven different factors across the crate,
//! shipping roughly a dozen unnamed greys: section headers landed at 3.4:1, key
//! hints at 2.4:1, the "You" label at 1.9:1. The eleven arrived one reasonable
//! call site at a time, so review discipline is not the fix — the fix is that
//! `.opacity()` on a text colour fails a test (`tests/text_tones.rs`), and a
//! tone that is genuinely needed becomes a fifth token here instead.
//!
//! Non-text opacity — washes, scrims, fills, hairlines, animation fades — is
//! untouched by the rule; those are what [`Theme::wash`] and
//! [`Theme::white_alpha`] are for.
//!
//! # Four status hues, one lightness (gh#173)
//!
//! Colour that means something — a state, not a surface — comes from ONE ramp:
//! four hues at [`Theme::STATUS_L`] lightness and [`Theme::STATUS_C`] chroma, so
//! no state shouts louder than another by accident. Before the anchor, amber sat
//! at L 0.828 and indigo at L 0.673, and the warning read twice as loud as the
//! accent even where it meant less; a fifth hue (pink) had been added for
//! "working" precisely because the amber read as alarm. Pink retired when amber
//! came down to the anchor.
//!
//! [`Status`] is the vocabulary, [`Theme::status`] the only function that turns
//! it into paint. Every state a board row, an agent row, a chat row or a tab dot
//! can be in maps into [`Status`] exactly once ([`Status::of_board`],
//! [`Status::of_agent`], [`Status::of_chat`]) — which is how the board pane and
//! the sidebar stopped disagreeing about the colour of a working agent (amber in
//! one, pink in the other, one keystroke apart on screen). States that mean
//! "nothing is happening" — ready, done, idle — map to `None` and spend no
//! colour at all: the absence of a hue is a state too.
//!
//! # Three radii and four type sizes (gh#174)
//!
//! Ten radii were in use — 4.5, 5, 6, 8, 10, 12, 16, 26 and full-round — which
//! is an inventory, not a scale, and the roundness never related to the box a
//! thing sat inside: a 5px action chip in a row inside a 12px card, a 26px
//! composer pill under a 16px bubble. Softness everywhere and structure
//! nowhere. Three steps replace it — [`Theme::RADIUS_CHIP`] 6,
//! [`Theme::RADIUS_ROW`] 10, [`Theme::RADIUS_CARD`] 14 — chosen four apart so
//! the **nesting rule** falls out of the scale: `inner = outer − padding`, with
//! the padding being [`Theme::NEST_GUTTER`], which is the gutter the cards
//! already used. A row at 10 inside a card at 14 reads as one object.
//!
//! # Hover is tone, selection is elevation (gh#175)
//!
//! Hover and selection used to be the same paint one notch apart — a 14% wash
//! against a 16% wash, with selection's only real telling a 1px inset ring at
//! 9% white. In a list you navigate by keyboard while the pointer rests
//! somewhere else — which is every board list, every space list, every
//! settings list — you could not tell which row was YOURS from which one
//! happened to be under the mouse.
//!
//! They are split by KIND, not by amount, and [`Theme::row`] is the one place
//! that answers it:
//!
//! - **Hover is a flat wash.** [`Theme::element_hover`], the same weak neutral
//!   tone a button gets. No edge, no structure, nothing but tone.
//! - **Selection lifts the row onto its own surface**, with a hairline round
//!   it ([`RowPaint::ring`], an inset shadow — nothing may paint BEHIND a
//!   glass row).
//!
//! Two channels, no ambiguity, and no colour is spent on "this one":
//! monochrome on purpose, because the status hues above mean STATE and
//! selection is not a state.
//!
//! Elevation only means something relative to the surface underneath, so the
//! row says what it sits on ([`Bed`]). On the shell a selected row lifts —
//! toward light in dark mode, to white in light mode. Inside a settings card
//! in light mode there is nowhere left to lift: the card is already the raised
//! white object on the page, so the row steps DOWN into it and the hairline
//! draws the edge of the well.
//!
//! `rounded_full()` survives in exactly one job — a dot, and the send button.
//! One round thing on screen, and it is the one you press. `tests/scale.rs`
//! holds the line: every remaining full-round needs a `round-ok:` marker
//! saying which of those it is.
//!
//! Type had ten sizes — 9, 9.5, 10, 10.5, 11, 11.5, 12, 12.5, 13, 13.5, 14, 15,
//! 16, 18, 21, 22, 30 — six of them inside the 520px board pane alone. A 0.5px
//! step is not a level of hierarchy, it is a different decision made on a
//! different day. Four sizes carry the UI ([`Theme::TEXT_CAPTION`] 11,
//! [`Theme::TEXT_DENSE`] 12, [`Theme::TEXT_BODY`] 13, [`Theme::TEXT_TITLE`]
//! 15), [`Theme::TEXT_PROSE`] 14/[`Theme::PROSE_LINE_HEIGHT`] 22 is reserved
//! for the transcript, and [`Theme::TEXT_FIGURE`] is the single display size
//! for a number shown as a number. Hierarchy past that comes from weight and
//! the four greys above — not from half-pixels.

use comet_proto::ChatIndicator;
use comet_proto::view::board::{AgentState, BoardState};
use comet_proto::view::status;
use gpui::{
    App, BoxShadow, Global, Hsla, InteractiveElement, Rgba, SharedString, Styled, hsla, point, px,
};
use serde::{Deserialize, Serialize};

/// What a state MEANS, in the only vocabulary the status ramp understands.
///
/// Four meanings, four hues — see the module docs. The types a state arrives in
/// (a board row's [`BoardState`], a live attempt's [`AgentState`], a chat's
/// [`ChatIndicator`]) each translate into this once, so two panes rendering the
/// same state cannot pick different paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// Stopped, and it needs a human: blocked, failed, errored.
    Blocked,
    /// Running on its own — an agent is working, in the board pane AND in the
    /// sidebar.
    Working,
    /// Finished and unlooked-at: review, a question, and the links and focus
    /// that lead there.
    Review,
    /// Settled: seen, healthy, online.
    Settled,
}

impl Status {
    /// A board row's state as a meaning. `None` for the states that spend no
    /// colour: `ready` is a queue entry (it reads as its own text) and `done` is
    /// history.
    pub fn of_board(state: BoardState) -> Option<Self> {
        match state {
            // One hue for both — the glyph tells a dead run from a gate.
            BoardState::Blocked | BoardState::Failed => Some(Status::Blocked),
            BoardState::Working => Some(Status::Working),
            BoardState::Review => Some(Status::Review),
            BoardState::Ready | BoardState::Done => None,
        }
    }

    /// A live attempt's state as a meaning — the board's vocabulary, since an
    /// agent row is a board row that moved into the sidebar (gh#103). It is the
    /// COARSE reading: a question and a corpse are both `blocked` here, told
    /// apart by the row's glyph.
    pub fn of_agent(state: AgentState) -> Self {
        match state {
            AgentState::Blocked | AgentState::Errored => Status::Blocked,
            AgentState::Working => Status::Working,
        }
    }

    /// A chat's display status as a meaning. `None` for idle — a chat nobody is
    /// waiting on is not a status, and its dot is a hairline, not a hue.
    ///
    /// This is the FINE reading, where a dot has no glyph to carry the
    /// difference: a question is the review hue (something wants your eyes, and
    /// the run is healthy), a dead run is the blocked hue. The Needs-you inbox
    /// splits them the same way and in the same colours (gh#122).
    pub fn of_chat(status: ChatIndicator) -> Option<Self> {
        match status {
            ChatIndicator::Working => Some(Status::Working),
            ChatIndicator::AwaitingInput => Some(Status::Review),
            ChatIndicator::Errored => Some(Status::Blocked),
            ChatIndicator::Completed => Some(Status::Settled),
            ChatIndicator::Idle => None,
        }
    }
}

/// What a list row SITS ON — because elevation is relative, and a row can only
/// lift away from something (gh#175).
///
/// Two beds, because the app has two: everything floating over the window, and
/// the settings pages' cards. Which one a row is in decides which way its
/// selection moves — see [`Theme::row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bed {
    /// The shell and everything over it: the sidebar's space/chat/agent lists,
    /// the board pane, session and terminal tabs, popovers, pickers, the
    /// palette. Dim in dark and bright-but-translucent in light, and in both
    /// there is room above for a selected row to lift into.
    Shell,
    /// Inside a settings card ([`crate::settings::widgets::section_card`]).
    /// In light that card is already the raised white object on the page: a
    /// row cannot lift out of it without competing with it, so it steps down
    /// instead. In dark the card is near-black and the row lifts as usual.
    Card,
}

/// The paint for one list row, in the two channels that cannot be mistaken for
/// each other: a flat wash for the pointer, a change of SURFACE plus a
/// hairline for the selection (gh#175).
///
/// Built by [`Theme::row`] — the one helper every list in the app asks. Apply
/// it whole with [`ListRow::list_row`], or read the fields when the row already
/// owns its `on_hover` (the session tabs track hover in view state).
#[derive(Debug, Clone)]
pub struct RowPaint {
    /// Background at rest.
    pub rest: Hsla,
    /// Background with the pointer over the row. For a selected row this is
    /// [`Self::rest`] plus the hover step, so hovering the row you already
    /// picked brightens IT rather than dropping it back to an unselected tone.
    pub hovered: Hsla,
    /// The selected row's hairline, as an INSET shadow: gpui paints those on
    /// top of the background, edges only. Drop shadows are filled rects
    /// painted behind the element and would show through a translucent row as
    /// an opaque plate. Empty for an unselected row — hover has no edge, and
    /// that is the whole point.
    pub ring: Vec<BoxShadow>,
    /// Text tone at rest: the bright tone when selected, the reading tone
    /// otherwise.
    pub text: Hsla,
    /// Text tone under the pointer — the bright tone, always.
    pub text_hovered: Hsla,
}

impl RowPaint {
    /// Paint a row with the pair: background (fading between [`Self::rest`]
    /// and [`Self::hovered`]), the selected hairline, the text tone, and the
    /// hover listener that drives the fade.
    ///
    /// `fade_key` must be unique app-wide and stable across frames — the
    /// element's own id string is the usual choice. gpui allows exactly one
    /// `on_hover` per element, so a row that tracks hover in view state reads
    /// the fields instead of calling this.
    pub fn apply<E: Styled + InteractiveElement>(
        self,
        el: E,
        fade_key: impl Into<SharedString>,
    ) -> E {
        let key = fade_key.into();
        let mut el = el
            .bg(crate::motion::hover_blend(&key, self.rest, self.hovered))
            .text_color(crate::motion::hover_blend(
                &key,
                self.text,
                self.text_hovered,
            ))
            .shadow(self.ring);
        el.interactivity()
            .on_hover(crate::motion::hover_listener(key));
        el
    }
}

/// `.list_row(…)` on any element: the whole hover/selected answer in one call.
///
/// The list rows of this app are written in a dozen files and were, before
/// gh#175, each assembling their own four lines of rest-wash / hover-blend /
/// selected-wash / ring. This is the one call that replaces them.
pub trait ListRow: Styled + InteractiveElement + Sized {
    /// Paint this element as a list row on `bed` — see [`Theme::row`] and
    /// [`RowPaint::apply`].
    fn list_row(
        self,
        theme: &Theme,
        bed: Bed,
        selected: bool,
        fade_key: impl Into<SharedString>,
    ) -> Self {
        theme.row(bed, selected).apply(self, fade_key)
    }
}

impl<E: Styled + InteractiveElement + Sized> ListRow for E {}

/// Which theme variant the app paints with. Persisted in [`crate::settings::UiSettings`]
/// (`theme` key) and overridable per-run with `COMET_THEME=dark|light`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// The default — comet's original always-dark monochrome.
    #[default]
    Dark,
    /// The inverted ramp: light surfaces, dark text, accents tuned for white.
    Light,
}

impl ThemeChoice {
    /// Every variant, in nav order (dark first — the default).
    pub const ALL: [ThemeChoice; 2] = [ThemeChoice::Dark, ThemeChoice::Light];

    /// `COMET_THEME=dark|light`; `None` when unset (callers fall back to the
    /// persisted setting).
    pub fn from_env() -> Option<Self> {
        match std::env::var("COMET_THEME").ok().as_deref() {
            Some("light") => Some(Self::Light),
            Some("dark") => Some(Self::Dark),
            _ => None,
        }
    }

    /// Row label + purpose copy for the Appearance page.
    pub fn describe(self) -> (&'static str, &'static str) {
        match self {
            ThemeChoice::Dark => (
                "Dark",
                "Comet's original palette — near-black surfaces, monochrome text.",
            ),
            ThemeChoice::Light => (
                "Light",
                "The same ramp inverted for bright environments — light surfaces, dark text.",
            ),
        }
    }
}

impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
        })
    }
}

/// The app theme. One concrete instance per variant — dark by default, light
/// opt-in (see [`ThemeChoice`]).
#[derive(Debug, Clone)]
pub struct Theme {
    /// True for the light variant. Picks the wash/hairline/glass direction
    /// everywhere paint is generated from a primitive ([`Self::wash`],
    /// [`Self::white_alpha`], [`Self::glass`]).
    pub light: bool,
    // ---- paint: neutral surfaces (oklch chroma 0) ----
    /// App background — oklch(0.145 0 0) ≡ `#0a0a0a` (dark).
    pub bg: Hsla,
    /// Panel / sidebar surface — one scale step up.
    pub surface: Hsla,
    /// Raised surface: popovers, dialogs, cards.
    pub surface_raised: Hsla,
    /// The hover wash, and the ONLY hover wash: a flat, weak, neutral tone for
    /// rows and buttons alike. Selection is not a heavier version of this —
    /// it is a different channel entirely ([`Theme::row`], gh#175).
    pub element_hover: Hsla,
    /// Hairline border — white at low alpha.
    pub border: Hsla,
    /// Stronger border for focused/raised edges.
    pub border_strong: Hsla,

    // ---- paint: text (four tones, never multiplied — see the module docs) ----
    /// Headings, titles, the selected row. ~16.9:1 on [`Self::bg`].
    pub text: Hsla,
    /// Body copy and unselected rows — the default reading tone. ~8.4:1.
    pub text_muted: Hsla,
    /// Labels, metadata, captions, key hints. ~5.1:1 — still AA body text.
    pub text_subtle: Hsla,
    /// Disabled controls and placeholders, and nothing else. ~3.5:1 — the
    /// floor, so anything a user is meant to READ sits at `text_subtle` or up.
    pub text_faint: Hsla,

    // ---- paint: the status ramp (four hues, one L, one C — see module docs) ----
    /// Accent — indigo, [`Status::Review`]'s hue: review, a question, links,
    /// focus, selection tint.
    pub accent: Hsla,
    /// The accent hue at fill weight — off the ramp on purpose: a filled button
    /// is not a status, and a status-weight fill under white text would not hold
    /// contrast. Same hue, so the two read as one colour.
    pub accent_strong: Hsla,
    /// Danger — red, [`Status::Blocked`]'s hue: errors, the stop button.
    pub danger: Hsla,
    /// Warning — amber, [`Status::Working`]'s hue: a running agent, offline
    /// notices.
    pub warning: Hsla,
    /// Settled — emerald, [`Status::Settled`]'s hue: finished chats, an online
    /// device, an active account.
    pub settled: Hsla,

    // ---- fonts ----
    /// UI font family (bundling of Geist lands with asset work; until then the
    /// text system falls back to the system sans when the family is missing).
    pub font_sans: SharedString,
    /// Monospace family for code/terminal.
    pub font_mono: SharedString,
    /// Explicit system fallbacks, for callers that want to skip the lookup.
    pub font_sans_fallback: SharedString,
    pub font_mono_fallback: SharedString,
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    /// Opaque elsewhere: Linux/Windows get no compositor-blur guarantee, and a
    /// merely transparent window would show raw desktop through the sidebar.
    /// Darkness matched by eye to a reference Electron app's dark glass. That
    /// scrim is 0.76 over `hsl(0 0% 3%)`, but it sits on Electron's
    /// `under-window` vibrancy MATERIAL, which pre-darkens the blur; our bare
    /// backdrop blur has no material layer, so the scrim runs heavier to land
    /// on the same perceived tone (see [`Theme::glass`]).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.90 } else { 1.0 };
    /// Main-panel header height (comet `h-11`) — in-card headers (changes pane).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs). Content
    /// rides [`Self::TITLEBAR_TOP_PAD`] lower than center so the air above
    /// matches the perceived gap to the inset card below (border + card body).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet (comet `h-6`) — the
    /// WorkingIndicator row; reserving it keeps the composer from shifting.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;

    // ---- three radii, and one full-round exception (gh#174) ----
    /// The innermost step: chips, badges, key caps, small icon buttons,
    /// avatars, swatches — anything that sits INSIDE a [`Self::RADIUS_ROW`] row.
    pub const RADIUS_CHIP: f32 = 6.0;
    /// The middle step: rows, menu items, inputs, tabs, popovers, tiles — the
    /// things that sit inside a [`Self::RADIUS_CARD`] card.
    pub const RADIUS_ROW: f32 = 10.0;
    /// The outermost step: cards, dialogs, sheets, message bubbles, the
    /// composer. Nothing is rounder than this except the send button.
    pub const RADIUS_CARD: f32 = 14.0;
    /// The gutter that makes the nesting rule true. `inner = outer − padding`:
    /// a [`Self::RADIUS_ROW`] row inset by this much inside a
    /// [`Self::RADIUS_CARD`] card keeps the two curves concentric, and the same
    /// step takes a chip out of a row. It is [`Self::SPACE_XS`] — the scale was
    /// chosen so the gutter the cards already used is the one the rule wants.
    pub const NEST_GUTTER: f32 = Self::RADIUS_CARD - Self::RADIUS_ROW;

    // ---- four type sizes, plus prose and one figure (gh#174) ----
    /// Captions: labels, metadata, key hints, timestamps, badge text — the
    /// smallest type that ships.
    pub const TEXT_CAPTION: f32 = 11.0;
    /// Dense rows: the board pane, the sidebar lists, tables, diff gutters —
    /// where many lines stack and every pixel of height is spent twice.
    pub const TEXT_DENSE: f32 = 12.0;
    /// UI body — the default. Buttons, menu items, form fields, section copy.
    pub const TEXT_BODY: f32 = 13.0;
    /// Titles: page headers, dialog titles, empty-state headings. The top of
    /// the UI ramp; anything louder is a matter of weight, not size.
    pub const TEXT_TITLE: f32 = 15.0;
    /// Prose, and only prose: rendered markdown, the message bubbles, and the
    /// composer you type them into. Reserved — a transcript is reading, not
    /// chrome, and it does not share the UI ramp.
    pub const TEXT_PROSE: f32 = 14.0;
    /// The line height [`Self::TEXT_PROSE`] is set on.
    pub const PROSE_LINE_HEIGHT: f32 = 22.0;
    /// A number shown AS a number: the stats tiles, the headline dispatch
    /// count, a login code read aloud. The one size off the UI ramp, because a
    /// figure at title size stops being a figure — and it is ONE size, not the
    /// 21 / 22 / 30 it replaces.
    pub const TEXT_FIGURE: f32 = 21.0;

    // ---- the status ramp (gh#173) ----
    // Defined in `comet-proto` (`view::status`), not here: the terminal app
    // paints the same meanings and must land on the same hues.
    /// The lightness every status hue is anchored to in dark. One number, so
    /// "how loud is this state" is decided by the state and never by its hue.
    pub const STATUS_L: f32 = status::L;
    /// The same anchor for light, walked down so the hues keep contrast on a
    /// near-white surface (~5:1, the same step the text ramp takes).
    pub const STATUS_L_LIGHT: f32 = status::L_LIGHT;
    /// The chroma every status hue carries.
    pub const STATUS_C: f32 = status::C;
    /// Blocked · failed · errored.
    pub const HUE_BLOCKED: f32 = status::BLOCKED;
    /// Working — an agent is running.
    pub const HUE_WORKING: f32 = status::WORKING;
    /// Review · a question · links · focus.
    pub const HUE_REVIEW: f32 = status::REVIEW;
    /// Settled · seen · online.
    pub const HUE_SETTLED: f32 = status::SETTLED;

    // ---- hover is tone, selection is elevation (gh#175) ----
    /// How far a selected row lifts off a DARK bed — the alpha of the
    /// soft-white plate it becomes. More than twice the hover wash, because
    /// it is a different surface and not more of the same one; and no more
    /// than that, because the sublines on a selected row are painted in
    /// [`Self::text_subtle`] and every point of lift costs them contrast (at
    /// this step they hold ~3.3:1, where the wash it replaces gave ~3.5:1).
    const SELECT_LIFT: f32 = 0.16;
    /// The same lift on a light SHELL: near-white, still translucent enough
    /// that the window vibrancy reads through the row.
    const SELECT_WHITE: f32 = 0.85;
    /// How far a selected row sinks INTO a white card, where lifting would
    /// only compete with the card's own elevation.
    const SELECT_SINK: f32 = 0.18;
    /// What the pointer adds ON TOP of a selected row: the same small step in
    /// whichever direction that row already moved.
    const SELECT_HOVER_STEP: f32 = 0.06;
    /// The selected row's hairline over a dark bed (white) and over a light
    /// one (black). Twice the 0.09 ring it replaces, because the ring is no
    /// longer a detail that distinguishes two near-identical washes — it is
    /// half the signal, and it has to read at a glance across a long list.
    /// The light ring runs heavier still: a white row on a near-white panel
    /// is carried almost entirely by its edge.
    const SELECT_EDGE: f32 = 0.18;
    const SELECT_EDGE_LIGHT: f32 = 0.22;

    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    /// The frost tint painted over the blurred window background (macOS
    /// glass). Darker than `surface` — matched to the reference dark
    /// vibrancy scrim: `hsl(0 0% 3%)` (#080808) at [`Self::GLASS_ALPHA`].
    /// On light the scrim inverts: white at a lighter alpha, so the blur
    /// reads as a bright frosted panel rather than a dark plate. On opaque
    /// platforms this IS the surface tone (no tint swap).
    pub fn glass(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                white_alpha(0.80)
            } else {
                grey(8).opacity(Self::GLASS_ALPHA)
            }
        } else {
            self.surface
        }
    }

    /// Build the dark theme — comet's original. The surface tones are sampled
    /// straight from the reference screenshots of the original app
    /// (docs/reference): main panel `#060606`, shell/sidebar `#0d0d0d`.
    pub fn dark() -> Self {
        Self {
            light: false,
            bg: grey(6),       // main panel — sampled #060606
            surface: grey(13), // shell / sidebar — sampled #0d0d0d
            surface_raised: neutral(0.235),
            // Half the 0.14 it replaces: hover no longer has to carry
            // "selected" too, so it can go back to being a hint (gh#175).
            element_hover: wash(0.07),
            border: white_alpha(0.08),
            border_strong: white_alpha(0.14),
            text: neutral(0.938),        // #ebebeb — 16.9:1 on bg
            text_muted: neutral(0.728),  // #a7a7a7 —  8.4:1
            text_subtle: neutral(0.598), // #7f7f7f —  5.1:1
            text_faint: neutral(0.508),  // #656565 —  3.5:1
            // The status ramp — one lightness, one chroma, four hues.
            accent: oklch(Self::STATUS_L, Self::STATUS_C, Self::HUE_REVIEW),
            accent_strong: oklch(0.62, 0.19, Self::HUE_REVIEW),
            danger: oklch(Self::STATUS_L, Self::STATUS_C, Self::HUE_BLOCKED),
            warning: oklch(Self::STATUS_L, Self::STATUS_C, Self::HUE_WORKING),
            settled: oklch(Self::STATUS_L, Self::STATUS_C, Self::HUE_SETTLED),
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// Build the light theme — the same ramp inverted: surfaces walk DOWN from
    /// a near-white app background, text walks up to near-black, and the
    /// accents move from the 400-shades to the 600-shades so they keep contrast
    /// on white. The default remains dark; this is the opt-in variant.
    pub fn light() -> Self {
        Self {
            light: true,
            bg: neutral(0.985),      // main panel — near-white
            surface: neutral(0.965), // shell / sidebar — one step down
            surface_raised: neutral(0.92),
            element_hover: black_wash(0.06),
            border: black_wash(0.10),
            border_strong: black_wash(0.16),
            // The same four contrast steps, walked the other way: 16.9 / 8.4 /
            // 5.1 / 3.5 against the near-white `bg`.
            text: neutral(0.212),        // #191919
            text_muted: neutral(0.412),  // #4b4b4b
            text_subtle: neutral(0.528), // #6b6b6b
            text_faint: neutral(0.619),  // #868686
            // The same ramp, one anchor lower so the hues hold on white.
            accent: oklch(Self::STATUS_L_LIGHT, Self::STATUS_C, Self::HUE_REVIEW),
            accent_strong: oklch(0.47, 0.19, Self::HUE_REVIEW),
            danger: oklch(Self::STATUS_L_LIGHT, Self::STATUS_C, Self::HUE_BLOCKED),
            warning: oklch(Self::STATUS_L_LIGHT, Self::STATUS_C, Self::HUE_WORKING),
            settled: oklch(Self::STATUS_L_LIGHT, Self::STATUS_C, Self::HUE_SETTLED),
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// The variant for a [`ThemeChoice`] — the bridge between the persisted
    /// preference and the painted theme.
    pub fn for_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Dark => Self::dark(),
            ThemeChoice::Light => Self::light(),
        }
    }

    /// Which choice this theme corresponds to (so the Appearance page can show
    /// the active variant without carrying state).
    pub fn choice(&self) -> ThemeChoice {
        if self.light {
            ThemeChoice::Light
        } else {
            ThemeChoice::Dark
        }
    }

    /// **The** answer to "what colour is this state" (gh#173). The board pane,
    /// the sidebar's agent rows, the chat rows and the tab dots all arrive here,
    /// so a state cannot be one colour in one pane and another colour a
    /// keystroke away. States with no [`Status`] paint no hue — see the module
    /// docs.
    pub fn status(&self, status: Status) -> Hsla {
        match status {
            Status::Blocked => self.danger,
            Status::Working => self.warning,
            Status::Review => self.accent,
            Status::Settled => self.settled,
        }
    }

    /// Interactive-state wash for THIS theme's surface tone: soft-white over
    /// dark, soft-black over light. Hover fades must rest on `wash(0.0)`,
    /// never transparent black, so the interpolation stays theme-toned.
    pub fn wash(&self, alpha: f32) -> Hsla {
        if self.light {
            black_wash(alpha)
        } else {
            wash(alpha)
        }
    }

    /// The hairline/wash primitive for this theme: white at low alpha over dark
    /// surfaces, black at low alpha over light ones (a white hairline on white
    /// would vanish).
    pub fn white_alpha(&self, alpha: f32) -> Hsla {
        if self.light {
            black_wash(alpha)
        } else {
            white_alpha(alpha)
        }
    }

    /// **The** answer to "is this row hovered or is it mine" (gh#175) — for
    /// every list in the app: the sidebar's spaces and chats and agents, the
    /// board's sections and rows, the tabs, the menus and pickers, the
    /// settings lists.
    ///
    /// The two states differ by KIND, not by amount. Hover is a flat wash and
    /// nothing else. Selection puts the row on its own surface and draws a
    /// hairline round it — a channel hover never uses, so a pointer resting on
    /// row 4 can never be confused with a keyboard cursor on row 9. No colour
    /// is spent either way: the status hues mean state, and selection is not a
    /// state.
    ///
    /// Which way the surface moves depends on `bed`, because elevation is
    /// relative to what the row sits on. Dark beds have headroom, so the row
    /// lifts toward light. A light shell lifts to white. A light settings card
    /// has nowhere left to lift — it IS the raised white object on the page —
    /// so the row steps down into it and the hairline draws the well.
    pub fn row(&self, bed: Bed, selected: bool) -> RowPaint {
        if !selected {
            return RowPaint {
                // Rest on a zero-alpha wash of THIS theme's tone, never on
                // transparent black: the hover fade interpolates from here and
                // must stay theme-toned the whole way (see [`Self::wash`]).
                rest: self.wash(0.0),
                hovered: self.element_hover,
                ring: Vec::new(),
                text: self.text_muted,
                text_hovered: self.text,
            };
        }
        let (rest, hovered, edge) = match (bed, self.light) {
            (Bed::Card, true) => (
                black_wash(Self::SELECT_SINK),
                black_wash(Self::SELECT_SINK + Self::SELECT_HOVER_STEP),
                black_wash(Self::SELECT_EDGE_LIGHT),
            ),
            (Bed::Shell, true) => (
                white_alpha(Self::SELECT_WHITE),
                white_alpha(Self::SELECT_WHITE + Self::SELECT_HOVER_STEP),
                black_wash(Self::SELECT_EDGE_LIGHT),
            ),
            (_, false) => (
                wash(Self::SELECT_LIFT),
                wash(Self::SELECT_LIFT + Self::SELECT_HOVER_STEP),
                white_alpha(Self::SELECT_EDGE),
            ),
        };
        RowPaint {
            rest,
            hovered,
            ring: hairline_ring(edge),
            text: self.text,
            text_hovered: self.text,
        }
    }

    /// The danger text/icon tone for THIS theme's error UI (error chips,
    /// failure notices): dark uses the light red-300, readable on the
    /// near-black chip; light uses the darker red-700 so it keeps contrast on
    /// white. Borders/washes stay on [`Self::danger`]; only the foreground
    /// inverts (a light tone on a light wash would wash out).
    pub fn danger_text(&self) -> Hsla {
        if self.light {
            oklch(0.505, 0.213, 27.518) // red-700
        } else {
            oklch(0.808, 0.114, 19.571) // red-300
        }
    }

    /// The warning text/icon tone for THIS theme's notice UI (offline notices):
    /// amber-200 in dark, the darker amber-700 in light — see [`Self::danger_text`].
    pub fn warning_text(&self) -> Hsla {
        if self.light {
            oklch(0.555, 0.163, 48.998) // amber-700
        } else {
            oklch(0.924, 0.12, 95.746) // amber-200
        }
    }

    /// The settled text/icon tone for THIS theme's presence UI (the "Active"
    /// badge, a copied id's confirmation): pale green on the near-black chip,
    /// the darker green on white — see [`Self::danger_text`]. On the ramp's hue,
    /// off its lightness, because this one sits ON a fill of its own colour.
    pub fn settled_text(&self) -> Hsla {
        if self.light {
            oklch(0.46, 0.13, Self::HUE_SETTLED)
        } else {
            oklch(0.88, 0.11, Self::HUE_SETTLED)
        }
    }

    /// The floating card tone (popovers, dialogs, palettes): over the frosted
    /// backdrop blur on macOS the card is a translucent tint the vibrancy reads
    /// through; elsewhere it is the opaque tone it composites to. Dark uses the
    /// near-black `#161616` plate; light inverts to a bright translucent white.
    pub fn float_card(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                white_alpha(0.72)
            } else {
                grey(0x16).opacity(0.65)
            }
        } else if self.light {
            self.surface_raised
        } else {
            grey(0x16)
        }
    }

    /// The four text tones, brightest first — for callers that walk the ramp
    /// rather than naming one step of it (the contrast tests).
    pub fn text_tones(&self) -> [Hsla; 4] {
        [
            self.text,
            self.text_muted,
            self.text_subtle,
            self.text_faint,
        ]
    }

    /// Read the theme global.
    pub fn of(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

fn system_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn system_mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A neutral (chroma 0) oklch tone as Hsla. Chroma 0 means r == g == b exactly,
/// so this goes straight to an achromatic Hsla (skipping the hue math avoids
/// float-noise saturation).
pub fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Interactive-state wash: TRANSLUCENT soft-white, with alphas high enough to
/// stay visible at the brightest backdrop the 0.90 glass scrim can produce
/// (~L 0.13 over pure white — a 12% wash still adds ~+24 luma there). Fully
/// opaque washes killed the glass and flashed dark mid-fade (user reports);
/// hover fades must rest on `wash(0.0)`, never transparent BLACK, so the
/// interpolation stays white-toned. The DARK-theme primitive — callers that
/// also serve light mode use [`Theme::wash`] instead.
pub fn wash(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.92, alpha)
}

/// The light-theme inverse of [`wash`]: soft-black, so hover/selected washes
/// darken a near-white surface instead of vanishing.
fn black_wash(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.0, alpha)
}

/// White at the given alpha — the dark-theme hairline/wash primitive (light
/// mode uses [`Theme::white_alpha`]).
pub fn white_alpha(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 1.0, alpha)
}

/// A 1px hairline round an element, as an INSET shadow: gpui paints those ON
/// TOP of the background, edges only — a border with zero layout cost. Drop
/// shadows are filled rects painted BEHIND the element, and behind a
/// translucent fill they showed straight through as an opaque plate with a
/// greyed ring (user report) — nothing may paint behind a glass row. The
/// selected row's edge ([`Theme::row`]) is the one caller that matters.
pub fn hairline_ring(color: Hsla) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color,
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(0.0),
        spread_radius: px(1.0),
        inset: true,
    }]
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ `#0d0d0d`)
/// — for surfaces matched against reference-screenshot samples.
pub fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to gpui Hsla.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
/// Reference: Björn Ottosson's OKLab definition (the same matrices CSS Color 4 uses).
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's Hsla convention).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

/// Source-over composite of a translucent tone onto an opaque one — what the
/// eye actually receives. A wash has no brightness of its own; comparing one
/// against another (a hover wash against a selected row's lift, say) is only
/// meaningful once both have landed on the surface they are painted on.
pub fn composite(over: Hsla, under: Hsla) -> Hsla {
    let (a, b) = (Rgba::from(over), Rgba::from(under));
    let t = over.a.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| y + (x - y) * t;
    let (h, s, l) = rgb_to_hsl(lerp(a.r, b.r), lerp(a.g, b.g), lerp(a.b, b.b));
    hsla(h, s, l, 1.0)
}

/// WCAG relative luminance of an OPAQUE color (alpha is ignored — a translucent
/// tone has no luminance of its own until it composites onto something).
pub fn relative_luminance(color: Hsla) -> f32 {
    let rgb = Rgba::from(color);
    let lin = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb.r) + 0.7152 * lin(rgb.g) + 0.0722 * lin(rgb.b)
}

/// WCAG 2.1 contrast ratio between two opaque colors, 1.0 (identical) to 21.0
/// (black on white). The text tones are chosen by this number and pinned by
/// this module's tests — see the module docs on why they are never
/// multiplied.
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Linear per-component mix of two colors (paint helper for the gradient spinner).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    // Mix through hue naively — both spinner endpoints sit close enough on the
    // wheel that shortest-arc handling isn't needed for our palette.
    hsla(
        lerp(a.h, b.h),
        lerp(a.s, b.s),
        lerp(a.l, b.l),
        lerp(a.a, b.a),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_u8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn neutral_950_is_0a0a0a() {
        // oklch(0.145 0 0) is Tailwind neutral-950, comet's app background.
        let rgb = srgb_u8(oklch_to_srgb(0.145, 0.0, 0.0));
        assert_eq!(rgb, [10, 10, 10]);
    }

    #[test]
    fn oklch_accents_match_reference() {
        // Reference values computed independently (CSS Color 4 matrices).
        let (l, c) = (Theme::STATUS_L, Theme::STATUS_C);
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 265.0)), [127, 168, 255]); // review
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 25.0)), [247, 133, 125]); // blocked
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 75.0)), [222, 156, 49]); // working
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 160.0)), [71, 197, 140]); // settled
    }

    #[test]
    fn the_status_ramp_is_one_lightness_and_one_chroma() {
        // The whole point of gh#173: a state's loudness is decided by the state,
        // not by which hue it happened to be given.
        for (theme, l) in [
            (Theme::dark(), Theme::STATUS_L),
            (Theme::light(), Theme::STATUS_L_LIGHT),
        ] {
            for (status, hue) in [
                (Status::Blocked, Theme::HUE_BLOCKED),
                (Status::Working, Theme::HUE_WORKING),
                (Status::Review, Theme::HUE_REVIEW),
                (Status::Settled, Theme::HUE_SETTLED),
            ] {
                assert_eq!(
                    theme.status(status),
                    oklch(l, Theme::STATUS_C, hue),
                    "{status:?} is off the ramp"
                );
            }
            // And it lands: the four hues sit within a quarter of one another in
            // measured luminance (dark 1.14x, light 1.23x — the light anchor
            // clips emerald a little). The old palette spread 1.96x: amber-400
            // at 0.56 against indigo-400 at 0.29, the "twice as loud" this ramp
            // exists to end.
            let lums: Vec<f32> = [
                Status::Blocked,
                Status::Working,
                Status::Review,
                Status::Settled,
            ]
            .into_iter()
            .map(|s| relative_luminance(theme.status(s)))
            .collect();
            let (lo, hi) = lums
                .iter()
                .fold((f32::MAX, 0.0f32), |(lo, hi), &v| (lo.min(v), hi.max(v)));
            assert!(hi / lo < 1.25, "status hues differ in loudness: {lums:?}");
        }
    }

    #[test]
    fn quiet_states_spend_no_colour() {
        // Ready, done and idle are states too — the ones with no hue.
        assert_eq!(Status::of_board(BoardState::Ready), None);
        assert_eq!(Status::of_board(BoardState::Done), None);
        assert_eq!(Status::of_chat(ChatIndicator::Idle), None);
    }

    #[test]
    fn a_working_agent_is_one_colour() {
        // The bug gh#173 names: amber in the board pane, pink in the sidebar.
        let t = Theme::dark();
        assert_eq!(
            t.status(Status::of_agent(AgentState::Working)),
            t.status(Status::of_chat(ChatIndicator::Working).unwrap())
        );
        assert_eq!(
            Status::of_board(BoardState::Working),
            Some(Status::of_agent(AgentState::Working))
        );
    }

    #[test]
    fn neutral_scale_is_ordered() {
        let t = Theme::dark();
        assert!(t.bg.l < t.surface.l);
        assert!(t.surface.l < t.surface_raised.l);
        assert!(t.surface_raised.l < t.text_faint.l);
        assert!(t.text_faint.l < t.text_subtle.l);
        assert!(t.text_subtle.l < t.text_muted.l);
        assert!(t.text_muted.l < t.text.l);
        // Monochrome: neutrals carry no saturation.
        for c in [t.bg, t.surface, t.surface_raised]
            .into_iter()
            .chain(t.text_tones())
        {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
    }

    /// The four tones, measured rather than eyeballed. Every tone clears its
    /// tier's floor against BOTH surface tones — `bg` (#060606, the main panel)
    /// and `surface` (#0d0d0d, the shell) — so no screen can put readable copy
    /// below AA. The predecessor of this test was a hand-measured table in an
    /// issue, which is how eleven call-site multipliers landed two of the app's
    /// most common tones under 3:1.
    #[test]
    fn text_tones_clear_their_contrast_floors() {
        // (name, target on `bg`, floor that BOTH surfaces must clear)
        const TIERS: [(&str, f32, f32); 4] = [
            ("text", 16.9, 15.0), // headings, titles, selected rows
            ("muted", 8.4, 7.5),  // body, unselected rows
            ("subtle", 5.1, 4.5), // labels, metadata, captions — AA body text
            ("faint", 3.5, 3.0),  // disabled and placeholders only
        ];
        for t in [Theme::dark(), Theme::light()] {
            let tones = t.text_tones();
            for (tone, (name, target, floor)) in tones.into_iter().zip(TIERS) {
                let on_bg = contrast_ratio(tone, t.bg);
                let on_surface = contrast_ratio(tone, t.surface);
                assert!(
                    (on_bg - target).abs() < 0.35,
                    "{name} is {on_bg:.2}:1 on bg, designed for {target}:1 (light={})",
                    t.light
                );
                assert!(
                    on_surface >= floor,
                    "{name} is {on_surface:.2}:1 on surface, floor {floor} (light={})",
                    t.light
                );
            }
            // The tones must read as LEVELS, not noise: each is at least 1.4x
            // the contrast of the one below it. Two greys landing two percent
            // apart is the failure this whole token set replaces.
            for pair in tones.windows(2) {
                let (hi, lo) = (contrast_ratio(pair[0], t.bg), contrast_ratio(pair[1], t.bg));
                assert!(hi / lo >= 1.4, "{hi:.2}:1 vs {lo:.2}:1 is not a step");
            }
        }
    }

    #[test]
    fn contrast_ratio_matches_wcag_anchors() {
        let white = hsla(0.0, 0.0, 1.0, 1.0);
        let black = hsla(0.0, 0.0, 0.0, 1.0);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);
        // Symmetric — the brighter color may be either argument.
        assert_eq!(contrast_ratio(white, black), contrast_ratio(black, white));
        // #767676 on white is the canonical 4.5:1 AA boundary.
        assert!((contrast_ratio(grey(0x76), white) - 4.54).abs() < 0.05);
    }

    #[test]
    fn hairlines_are_white_and_washes_are_mid_grey() {
        let t = Theme::dark();
        // Hairlines stay white — they only need to read on dark surfaces.
        for c in [t.border, t.border_strong] {
            assert_eq!(c.l, 1.0, "hairlines are white");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        // The hover wash is translucent soft-white with enough alpha to read
        // at the glass scrim's brightness ceiling.
        assert_eq!(t.element_hover.l, 0.92, "washes are soft-white");
        assert!(
            t.element_hover.a >= 0.05 && t.element_hover.a < 0.35,
            "alpha in band, got {}",
            t.element_hover.a
        );
        assert!(t.border.a < t.border_strong.a);
    }

    /// The gh#175 rule, and the test that replaces the one asserting hover
    /// EQUALS the active fill: the two states must differ in kind.
    #[test]
    fn hover_is_tone_and_selection_is_elevation() {
        for t in [Theme::dark(), Theme::light()] {
            for bed in [Bed::Shell, Bed::Card] {
                let idle = t.row(bed, false);
                let picked = t.row(bed, true);

                // Hover is tone and nothing else — a flat wash with no edge,
                // over a row that paints nothing at rest.
                assert!(
                    idle.ring.is_empty(),
                    "hover must draw no structure (light={}, {bed:?})",
                    t.light
                );
                assert_eq!(idle.rest.a, 0.0, "an untouched row paints nothing");
                assert_eq!(
                    idle.hovered, t.element_hover,
                    "a row's hover is the app's one hover wash"
                );

                // Selection is the other channel: its own surface, with a
                // hairline round it that hover never has.
                assert_eq!(picked.ring.len(), 1, "a selected row is edged");
                assert!(
                    picked.ring[0].inset,
                    "nothing may paint BEHIND a translucent row"
                );

                // And the surface really is a different one, not a heavier
                // dose of the hover wash: the step from hover to selected is
                // bigger than the step from rest to hover.
                let on = |c: Hsla| composite(c, t.surface);
                let (rest, hover, sel) = (on(idle.rest), on(idle.hovered), on(picked.rest));
                assert!(
                    contrast_ratio(sel, hover) > contrast_ratio(hover, rest),
                    "selection reads as more-hover, not as elevation \
                     (light={}, {bed:?}): {:.3} vs {:.3}",
                    t.light,
                    contrast_ratio(sel, hover),
                    contrast_ratio(hover, rest)
                );

                // Elevation is relative to the bed. Everything lifts except a
                // row inside a light card, where there is nowhere left to go.
                let down = t.light && bed == Bed::Card;
                let lifted = relative_luminance(sel) > relative_luminance(hover);
                assert_eq!(
                    lifted, !down,
                    "wrong direction off the bed (light={}, {bed:?})",
                    t.light
                );

                // A row that lifts costs the metadata printed on it some
                // contrast, so the lift is bounded by what those sublines can
                // pay. This floor is why selection leans on its hairline
                // rather than on ever more wash.
                assert!(
                    contrast_ratio(t.text_subtle, sel) >= 3.0,
                    "sublines on the selected row read at only {:.2}:1 \
                     (light={}, {bed:?})",
                    contrast_ratio(t.text_subtle, sel),
                    t.light
                );

                // No colour is spent on "this one" — the status hues mean
                // state, and selection is not a state.
                for c in [idle.rest, idle.hovered, picked.rest, picked.hovered] {
                    assert_eq!(c.s, 0.0, "selection must spend no colour");
                }
                assert_eq!(picked.ring[0].color.s, 0.0);
            }
        }
    }

    #[test]
    fn a_selected_row_inside_a_white_card_is_unmistakable() {
        // The subtle case (gh#175): the settings cards are near-white in
        // light mode, so the row steps DOWN and its hairline draws the well.
        let t = Theme::light();
        let picked = t.row(Bed::Card, true);
        let hovered = t.row(Bed::Card, false).hovered;
        let card = t.surface;
        let (sel, hov) = (composite(picked.rest, card), composite(hovered, card));
        assert!(relative_luminance(sel) < relative_luminance(card));
        // Told apart from the merely-hovered row by the fill alone...
        assert!(
            contrast_ratio(sel, hov) > 1.25,
            "only {:.3}:1 between selected and hovered",
            contrast_ratio(sel, hov)
        );
        // ...and again by the edge, which reads against the row's own fill.
        assert!(contrast_ratio(composite(picked.ring[0].color, sel), sel) > 1.15);
    }

    #[test]
    fn composite_lands_between_the_two_tones() {
        let white = hsla(0.0, 0.0, 1.0, 1.0);
        let black = hsla(0.0, 0.0, 0.0, 1.0);
        // A fully transparent wash leaves the surface alone; an opaque one
        // replaces it; half lands halfway.
        assert_eq!(composite(white.opacity(0.0), black), black);
        assert_eq!(composite(white, black), white);
        assert!((composite(white.opacity(0.5), black).l - 0.5).abs() < 0.01);
    }

    #[test]
    fn accent_hues_land_in_their_bands() {
        let t = Theme::dark();
        // Hsla hue is 0..1 of the wheel. Indigo ≈ 230-250°, red < 15°, amber ≈ 40-55°.
        let deg = |c: Hsla| c.h * 360.0;
        assert!(
            (215.0..265.0).contains(&deg(t.accent)),
            "indigo hue {}",
            deg(t.accent)
        );
        assert!(
            deg(t.danger) < 15.0 || deg(t.danger) > 345.0,
            "red hue {}",
            deg(t.danger)
        );
        assert!(
            (35.0..60.0).contains(&deg(t.warning)),
            "amber hue {}",
            deg(t.warning)
        );
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = hsla(0.0, 0.0, 0.0, 1.0);
        let b = hsla(0.5, 1.0, 1.0, 0.0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let mid = mix(a, b, 0.5);
        assert!((mid.l - 0.5).abs() < 1e-6 && (mid.a - 0.5).abs() < 1e-6);
        // Out-of-range t clamps.
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn layout_numbers_match_comet() {
        assert_eq!(Theme::HEADER_HEIGHT, 44.0); // h-11
        assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0); // h-6
    }

    #[test]
    fn the_radius_scale_makes_the_nesting_rule_true() {
        // gh#174: three steps, each one gutter apart, so `inner = outer −
        // padding` is arithmetic rather than a thing to remember. The ten-radius
        // inventory that preceded this had no such relation: a 5px chip in a
        // 12px card left the two curves fighting.
        assert_eq!(
            Theme::RADIUS_CARD - Theme::RADIUS_ROW,
            Theme::RADIUS_ROW - Theme::RADIUS_CHIP,
            "the scale must be evenly stepped or the rule only works one level deep"
        );
        assert_eq!(Theme::NEST_GUTTER, Theme::RADIUS_ROW - Theme::RADIUS_CHIP);
        // And the gutter is the one the cards already used — the scale was
        // fitted to the layout, not the other way round.
        assert_eq!(Theme::NEST_GUTTER, Theme::SPACE_XS);
        assert_eq!(
            [
                Theme::RADIUS_CHIP,
                Theme::RADIUS_ROW,
                Theme::RADIUS_CARD,
                Theme::NEST_GUTTER
            ],
            [6.0, 10.0, 14.0, 4.0]
        );
    }

    #[test]
    fn the_type_scale_is_four_sizes_and_two_reservations() {
        // Four UI sizes, ordered and never closer than a whole pixel — a 0.5px
        // step is not a level of hierarchy (gh#174).
        let ui = [
            Theme::TEXT_CAPTION,
            Theme::TEXT_DENSE,
            Theme::TEXT_BODY,
            Theme::TEXT_TITLE,
        ];
        assert_eq!(ui, [11.0, 12.0, 13.0, 15.0]);
        for pair in ui.windows(2) {
            assert!(
                pair[1] - pair[0] >= 1.0,
                "{} and {} are the same decision made twice",
                pair[0],
                pair[1]
            );
        }
        // Prose is reserved: it sits between the dense rows and the titles on
        // purpose, and carries its own line height.
        assert_eq!((Theme::TEXT_PROSE, Theme::PROSE_LINE_HEIGHT), (14.0, 22.0));
        // Prose reads a step above UI body and a step below a title, and the
        // one display size sits above the whole UI ramp.
        let full = [
            Theme::TEXT_BODY,
            Theme::TEXT_PROSE,
            Theme::TEXT_TITLE,
            Theme::TEXT_FIGURE,
        ];
        assert!(
            full.windows(2).all(|p| p[0] < p[1]),
            "the scale is not ordered: {full:?}"
        );
    }

    // ---- light variant ----

    #[test]
    fn light_neutral_scale_is_inverted_ordered() {
        let t = Theme::light();
        // The light ramp walks the OTHER way: backgrounds are the lightest
        // tones, text the darkest.
        assert!(t.bg.l > t.surface.l);
        assert!(t.surface.l > t.surface_raised.l);
        assert!(t.surface_raised.l > t.text_faint.l);
        assert!(t.text_faint.l > t.text_subtle.l);
        assert!(t.text_subtle.l > t.text_muted.l);
        assert!(t.text_muted.l > t.text.l);
        // Monochrome: neutrals carry no saturation, same as dark.
        for c in [t.bg, t.surface, t.surface_raised]
            .into_iter()
            .chain(t.text_tones())
        {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
        // And the two variants share their ramp structure — a surface that is
        // raised in dark is raised in light, text that is prominent in dark is
        // prominent in light.
        let d = Theme::dark();
        assert_eq!(d.bg.l < d.surface.l, t.bg.l > t.surface.l);
    }

    #[test]
    fn light_hairlines_are_black_and_washes_are_soft_black() {
        let t = Theme::light();
        // Hairlines flip to black — a white hairline would vanish on white.
        for c in [t.border, t.border_strong] {
            assert_eq!(c.l, 0.0, "hairlines are black");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        // The hover wash is a translucent soft-black that darkens a near-white
        // surface.
        assert_eq!(t.element_hover.l, 0.0, "washes are soft-black");
        assert!(
            t.element_hover.a >= 0.05 && t.element_hover.a < 0.35,
            "alpha in band, got {}",
            t.element_hover.a
        );
        assert!(t.border.a < t.border_strong.a);
        // The theme primitives mirror the fields.
        assert_eq!(t.wash(0.14).l, 0.0);
        assert_eq!(t.white_alpha(0.09).l, 0.0);
        // A selected row on the light shell is the one wash that runs the
        // other way: it lifts to WHITE, edged in black so the lift reads.
        let picked = t.row(Bed::Shell, true);
        assert_eq!(picked.rest.l, 1.0);
        assert_eq!(picked.ring[0].color.l, 0.0);
    }

    #[test]
    fn light_accent_hues_land_in_their_bands() {
        let t = Theme::light();
        let deg = |c: Hsla| c.h * 360.0;
        assert!(
            (215.0..265.0).contains(&deg(t.accent)),
            "indigo hue {}",
            deg(t.accent)
        );
        assert!(
            deg(t.danger) < 15.0 || deg(t.danger) > 345.0,
            "red hue {}",
            deg(t.danger)
        );
        assert!(
            (35.0..60.0).contains(&deg(t.warning)),
            "amber hue {}",
            deg(t.warning)
        );
    }

    #[test]
    fn light_accents_are_the_darker_shades() {
        // Light mode walks the ramp down one anchor so the hues keep contrast on
        // white (dark sits at the bright anchor). Pinned to this crate's own
        // oklch conversion (which differs a hair from CSS Color 4's matrices).
        let (l, c) = (Theme::STATUS_L_LIGHT, Theme::STATUS_C);
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 265.0)), [73, 109, 195]);
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 25.0)), [181, 74, 70]);
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 75.0)), [160, 98, 0]);
        assert_eq!(srgb_u8(oklch_to_srgb(l, c, 160.0)), [0, 137, 84]);
        // Every hue on the light ramp is visibly darker than the dark theme's,
        // and the strong accent is darker still than the accent it fills for.
        let (t, dark) = (Theme::light(), Theme::dark());
        for status in [
            Status::Blocked,
            Status::Working,
            Status::Review,
            Status::Settled,
        ] {
            assert!(
                relative_luminance(t.status(status)) < relative_luminance(dark.status(status)),
                "{status:?} did not darken for light"
            );
        }
        for t in [Theme::light(), Theme::dark()] {
            assert!(relative_luminance(t.accent_strong) < relative_luminance(t.accent));
        }
    }

    #[test]
    fn error_notice_text_inverts_with_theme() {
        // Error/notice bars: dark keeps the pale 300/200 tones (readable on the
        // near-black chip), light uses the darker 700-shades so the text keeps
        // contrast on white.
        let dark = Theme::dark();
        let light = Theme::light();
        assert!(light.danger_text().l < dark.danger_text().l);
        assert!(light.warning_text().l < dark.warning_text().l);
        // Dark mode pins the previous hardcoded tones unchanged.
        assert_eq!(dark.danger_text(), oklch(0.808, 0.114, 19.571)); // red-300
        assert_eq!(dark.warning_text(), oklch(0.924, 0.12, 95.746)); // amber-200
        // Light mode uses the darker equivalents.
        assert_eq!(light.danger_text(), oklch(0.505, 0.213, 27.518)); // red-700
        assert_eq!(light.warning_text(), oklch(0.555, 0.163, 48.998)); // amber-700
        // The light tones still hold contrast against the light surfaces they
        // sit on (darker than the chip fills, like the rest of the ramp).
        assert!(light.danger_text().l < light.surface.l);
        assert!(light.warning_text().l < light.surface.l);
    }

    #[test]
    fn light_surfaces_stay_legible() {
        let t = Theme::light();
        // Text on every surface keeps real contrast (dark text on light fills).
        for (fg, bg) in [
            (t.text, t.bg),
            (t.text, t.surface),
            (t.text, t.surface_raised),
            (t.text_muted, t.bg),
            (t.text_subtle, t.surface),
            (t.text_faint, t.surface),
        ] {
            assert!(fg.l < bg.l - 0.15, "fg {} vs bg {}", fg.l, bg.l);
        }
        // Accents contrast against white: darker than the surface they sit on.
        assert!(t.accent.l < t.surface.l);
        assert!(t.danger.l < t.surface.l);
        assert!(t.warning.l < t.surface.l);
    }

    #[test]
    fn theme_choice_round_trips() {
        // The persisted form is `theme: "light"` / `"dark"` (camelCase file,
        // lowercase values).
        assert_eq!(serde_json::from_str::<ThemeChoice>("\"light\"").unwrap(), ThemeChoice::Light);
        assert_eq!(serde_json::from_str::<ThemeChoice>("\"dark\"").unwrap(), ThemeChoice::Dark);
        assert_eq!(
            serde_json::to_string(&ThemeChoice::Light).unwrap(),
            "\"light\""
        );
        // Default is dark.
        assert_eq!(ThemeChoice::default(), ThemeChoice::Dark);
        assert!(!Theme::for_choice(ThemeChoice::Dark).light);
        assert!(Theme::for_choice(ThemeChoice::Light).light);
        assert_eq!(Theme::dark().choice(), ThemeChoice::Dark);
        assert_eq!(Theme::light().choice(), ThemeChoice::Light);
    }

    #[test]
    fn theme_choice_from_env() {
        // Reads COMET_THEME; unset or unknown values yield None (callers fall
        // back to the persisted choice).
        unsafe {
            std::env::set_var("COMET_THEME", "light");
        }
        assert_eq!(ThemeChoice::from_env(), Some(ThemeChoice::Light));
        unsafe {
            std::env::set_var("COMET_THEME", "dark");
        }
        assert_eq!(ThemeChoice::from_env(), Some(ThemeChoice::Dark));
        unsafe {
            std::env::set_var("COMET_THEME", "sepia");
        }
        assert_eq!(ThemeChoice::from_env(), None);
        unsafe {
            std::env::remove_var("COMET_THEME");
        }
        assert_eq!(ThemeChoice::from_env(), None);
    }
}
