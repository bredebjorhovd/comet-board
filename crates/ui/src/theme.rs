//! Monochrome theme — concrete values, no indirection.
//!
//! Colors are precomputed from an oklch-derived neutral scale (perceptually even
//! lightness steps; the same scale comet's Tailwind theme used) into gpui [`Hsla`].
//! **Numbers drive layout, colors are paint**: layout constants live here as plain
//! numbers and never depend on which color is painted.
//!
//! Two variants, [`Theme::dark`] (the default — comet is always-dark by design)
//! and [`Theme::light`], which declares its OWN values rather than inverting
//! dark's (see below). The chosen variant is installed as a gpui [`Global`] at
//! boot (`cx.set_global(Theme::dark())`) and can be flipped live from the
//! Appearance settings page; read with [`Theme::of`]. Hairlines and interactive
//! washes are white at low alpha in dark and cool ink at low alpha in light —
//! see [`Theme::white_alpha`] / [`Theme::wash`].
//!
//! # Light is a design, not an inversion (gh#177)
//!
//! Light mode used to be dark's ramp read backwards, and four things do not
//! survive that trip:
//!
//! 1. **Elevation.** Dark walks `#060606 → #0d0d0d → #1e1e1e`, brighter as it
//!    rises. Inverted, light walked `#fafafa → #f3f3f3 → #e4e4e4` — a popover
//!    DARKER than the page behind it, which reads as a hole. Light elevates
//!    toward white and lets shadow do the lifting: the shell is the ground
//!    ([`Theme::surface`]), the main panel is paper ([`Theme::bg`]), and a
//!    float is white ([`Theme::surface_raised`] / [`Theme::float_card`]) under
//!    [`Theme::float_shadow`].
//! 2. **Your own message.** The user bubble painted `surface_raised`, which
//!    inverted to a mid-grey slab in the middle of a white page. It has its own
//!    token now — [`Theme::bubble`], the shallowest step the paper can take
//!    plus a hairline — because "mine" is a distinction, not an elevation.
//! 3. **Washes are not perceptually symmetric.** White at 14% over near-black
//!    is a lift; ink at 14% over near-white is a scrim. Light washes run at
//!    [`Theme::LIGHT_WASH_SCALE`] of the alpha a call site names, and they are
//!    made of cool [`ink`] rather than black so they tint instead of dirtying.
//!    (The selection washes are the exception and keep their full alpha: they
//!    were tuned against the light bed in the first place — see [`Theme::row`],
//!    which answers hover-versus-selected for every list.)
//! 4. **Contrast.** The four text tones are measured against every light
//!    surface, not just the page — see `text_tones_clear_their_contrast_floors`.
//!
//! And one macOS-only: light frost used to be plain white at 80% over the
//! blurred desktop, so the wallpaper tinted the sidebar. [`Theme::glass`] is a
//! tinted scrim now, at dark's neutralising alpha.
//!
//! Every light neutral carries a trace of blue ([`Theme::LIGHT_NEUTRAL_HUE`]) —
//! a pure grey beside white makes the white read yellow. The one exception is
//! `surface_raised`, which IS white: it is the thing the trace exists to keep
//! looking white.
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
                "Its own palette for bright rooms — paper-white panels, cool neutrals, shadow for lift.",
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
    // ---- paint: neutral surfaces ----
    // Dark's are achromatic; light's carry [`Self::LIGHT_NEUTRAL_HUE`]'s trace
    // of blue (see the module docs).
    /// The main panel. Dark's darkest tone (`#060606`, the inset well); light's
    /// paper, one step ABOVE the shell it floats on.
    pub bg: Hsla,
    /// Panel / sidebar surface — the shell the main panel is inset into. Dark
    /// walks UP from `bg` to reach it, light walks DOWN: it is the ground.
    pub surface: Hsla,
    /// A card raised off the main panel: the settings sections, the login and
    /// workspace cards. Distinct from [`Self::surface`] because that token is
    /// doing two jobs that only coincide in dark — the shell is one step ABOVE
    /// `bg` there, so "the shell tone" and "a card tone" are the same number.
    /// In light the shell is BELOW the paper and a card is above it, and using
    /// one for the other is how a settings card ends up darker than the page
    /// it sits on (gh#177).
    pub card: Hsla,
    /// Raised surface: popovers, dialogs, floats, the composer well. The top of
    /// the elevation ramp in BOTH variants — white in light, where the lift
    /// comes from [`Self::float_shadow`] rather than from more tone.
    pub surface_raised: Hsla,
    /// Hover tone for an OPAQUE raised surface — the one case a translucent
    /// wash cannot serve, because the surface it would sit on is already the
    /// top of the ramp. Brighter than `surface_raised` in dark, dimmer in light.
    pub surface_raised_hover: Hsla,
    /// Your own message. Not an elevation — a distinction, so it is a tint
    /// rather than a step on the surface ramp (gh#177: inverted, the old
    /// `surface_raised` bubble was a mid-grey slab on a white page).
    pub bubble: Hsla,
    /// The hairline around [`Self::bubble`]. Transparent in dark, where the
    /// tone carries the shape on its own.
    pub bubble_border: Hsla,
    /// The hover wash, and the ONLY hover wash: a flat, weak, neutral tone for
    /// rows and buttons alike. Selection is not a heavier version of this —
    /// it is a different channel entirely ([`Theme::row`], gh#175).
    pub element_hover: Hsla,
    /// Hairline border — white at low alpha in dark, cool ink in light.
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

    // ---- light is its own design (gh#177) ----
    /// The hue every light neutral carries a trace of. A pure grey beside white
    /// makes the white read yellow, so the light surfaces, hairlines, washes
    /// and text tones are all mixed at this hue with a chroma small enough that
    /// nobody would call them blue — and `surface_raised` stays pure white,
    /// which is the point.
    pub const LIGHT_NEUTRAL_HUE: f32 = 255.0;
    /// The fraction of a call site's named alpha a light wash actually paints.
    /// The alphas across the crate were chosen for soft-white over near-black,
    /// and ink over near-white is not their mirror: white at 14% over `#060606`
    /// is a gentle lift, black at 14% over `#f9fafc` is a grey scrim on the
    /// row. One scale here beats retuning sixty call sites — and the tone is
    /// [`ink`], not black, so what lands is a tint rather than a smudge.
    pub const LIGHT_WASH_SCALE: f32 = 0.65;

    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    /// The frost tint painted over the blurred window background (macOS
    /// glass). Darker than `surface` — matched to the reference dark
    /// vibrancy scrim: `hsl(0 0% 3%)` (#080808) at [`Self::GLASS_ALPHA`].
    ///
    /// Light gets a TINTED scrim at the same neutralising strength, not a
    /// transparent one (gh#177): plain white at 80% let whatever wallpaper sat
    /// behind the window tint the sidebar, because white has no colour of its
    /// own to win with — dark's near-black neutralises by being far from
    /// everything, and the light scrim has to earn that with its own tone.
    /// On opaque platforms this IS the surface tone (no tint swap).
    pub fn glass(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                cool(0.960, 0.006).opacity(0.93)
            } else {
                grey(8).opacity(Self::GLASS_ALPHA)
            }
        } else {
            self.surface
        }
    }

    /// The lift under a floating card — popovers, dialogs, palettes, the
    /// suggestion list. Dark keeps Tailwind's `shadow-lg` (a black shadow on a
    /// near-black ground barely registers; the card's own tone does the work).
    /// Light is where a shadow is the whole mechanism, so it gets a real one:
    /// a tight contact shadow under a wide soft cast, both in cool [`ink`]
    /// rather than black so they read as shade and not as grime (gh#177).
    pub fn float_shadow(&self) -> Vec<gpui::BoxShadow> {
        let shadow = |y: f32, blur: f32, spread: f32, color: Hsla| gpui::BoxShadow {
            color,
            offset: gpui::point(gpui::px(0.0), gpui::px(y)),
            blur_radius: gpui::px(blur),
            spread_radius: gpui::px(spread),
            inset: false,
        };
        if self.light {
            vec![
                shadow(1.0, 2.0, 0.0, ink(0.08)),
                shadow(10.0, 24.0, -6.0, ink(0.16)),
            ]
        } else {
            vec![
                shadow(10.0, 15.0, -3.0, hsla(0.0, 0.0, 0.0, 0.1)),
                shadow(4.0, 6.0, -4.0, hsla(0.0, 0.0, 0.0, 0.1)),
            ]
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
            card: grey(13),    // a card and the shell coincide in dark
            surface_raised: neutral(0.235),
            surface_raised_hover: neutral(0.29),
            bubble: neutral(0.235),
            bubble_border: gpui::transparent_black(),
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

    /// Build the light theme — its OWN declared values, not dark's ramp read
    /// backwards (gh#177; the module docs say which four things do not survive
    /// the inversion). The shape of it:
    ///
    /// - **Elevation runs toward white.** The shell is the ground
    ///   (`#edf0f4`), the main panel is paper on it (`#f9fafc`), and a float
    ///   is white — with [`Self::float_shadow`], not more tone, doing the
    ///   lifting.
    /// - **Neutrals lean blue.** Every tone here is [`cool`] at
    ///   [`Self::LIGHT_NEUTRAL_HUE`]; only `surface_raised` is pure white, so
    ///   the white it puts on screen stays white instead of reading yellow.
    /// - **Ink, not black.** Washes and hairlines are [`ink`] — a cool
    ///   near-black — at [`Self::LIGHT_WASH_SCALE`] of the named alpha.
    /// - **Accents drop an anchor** so the four hues keep contrast on paper.
    ///
    /// The default remains dark; this is the opt-in variant.
    pub fn light() -> Self {
        Self {
            light: true,
            bg: cool(0.986, 0.003), // #f9fafc — the paper, ABOVE the shell
            surface: cool(0.955, 0.006), // #edf0f4 — shell / sidebar, the ground
            // A card is above the paper, where the shell is below it — the one
            // place the two jobs `surface` was doing come apart.
            card: cool(1.0, 0.0),           // #ffffff, carried by its hairline
            surface_raised: cool(1.0, 0.0), // #ffffff — the top of the ramp
            surface_raised_hover: cool(0.965, 0.004), // white has to hover DOWN
            // "Mine" is a WELL in the paper, drawn by its edge — the shallowest
            // step that still reads, because the tone that carries it is the
            // hairline. Monochrome, like every other non-state surface: the
            // hues mean state (gh#173) and "who said this" is not one.
            bubble: cool(0.965, 0.005), // #f1f3f7
            bubble_border: ink(0.14),
            // gh#175's weight, in this theme's ink rather than in black.
            element_hover: ink(0.06),
            // Hairlines keep a full alpha where washes are scaled: a wash that
            // floods can afford to be quieter, a line that has to be SEEN
            // cannot.
            border: ink(0.13),
            border_strong: ink(0.20),
            // The same four contrast steps, measured against the light
            // surfaces: 16.9 / 8.4 / 5.1 / 3.5 on `bg`, and every one of them
            // still clears its floor on the ground and in the bubble.
            text: cool(0.213, 0.008),        // #17191d
            text_muted: cool(0.412, 0.008),  // #484b4f
            text_subtle: cool(0.529, 0.008), // #686c70
            text_faint: cool(0.620, 0.008),  // #83868b
            // The status ramp, one anchor lower so the hues hold on paper.
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
    /// dark, cool [`ink`] over light — at [`Self::LIGHT_WASH_SCALE`] of the
    /// named alpha, because the two directions are not each other's mirror
    /// (gh#177). Hover fades must rest on `wash(0.0)`, never transparent black,
    /// so the interpolation stays theme-toned — and the scale keeps 0 at 0.
    pub fn wash(&self, alpha: f32) -> Hsla {
        if self.light {
            ink(alpha * Self::LIGHT_WASH_SCALE)
        } else {
            wash(alpha)
        }
    }

    /// The hairline primitive for this theme: white at low alpha over dark
    /// surfaces, cool [`ink`] at low alpha over light ones (a white hairline on
    /// white would vanish). Unlike [`Self::wash`] this does NOT scale: a wash
    /// that floods a whole row can afford to be quieter on light, a line whose
    /// entire job is to be seen cannot.
    pub fn white_alpha(&self, alpha: f32) -> Hsla {
        if self.light {
            ink(alpha)
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
    ///
    /// The light washes here are [`ink`] rather than black (gh#177): a sink
    /// and an edge made of black over a near-white bed read as grime, and this
    /// theme's neutrals lean cool. The alphas are gh#175's, unchanged — a
    /// selection is one of the few washes that was tuned against the light bed
    /// in the first place, so [`Self::LIGHT_WASH_SCALE`] does not apply.
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
                ink(Self::SELECT_SINK),
                ink(Self::SELECT_SINK + Self::SELECT_HOVER_STEP),
                ink(Self::SELECT_EDGE_LIGHT),
            ),
            (Bed::Shell, true) => (
                white_alpha(Self::SELECT_WHITE),
                white_alpha(Self::SELECT_WHITE + Self::SELECT_HOVER_STEP),
                ink(Self::SELECT_EDGE_LIGHT),
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
    /// near-black `#161616` plate.
    ///
    /// Light is WHITE, and near-opaque (gh#177). A float has to sit above the
    /// page it covers, and the old translucent white did the opposite twice
    /// over: it sat below (the inverted ramp made it darker than the page) and
    /// it took its colour from whatever wallpaper the blur happened to catch.
    /// The lift comes from [`Self::float_shadow`] instead.
    pub fn float_card(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                cool(0.995, 0.002).opacity(0.95)
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

/// A light-mode neutral: an oklch tone carrying [`Theme::LIGHT_NEUTRAL_HUE`]'s
/// trace of blue. `cool(l, 0.0)` is exactly [`neutral`] — which is how
/// `surface_raised` gets to be pure white while everything around it leans
/// cool, so the white reads as white.
pub fn cool(lightness: f32, chroma: f32) -> Hsla {
    oklch(lightness, chroma, Theme::LIGHT_NEUTRAL_HUE)
}

/// The light theme's ink — what a hairline, a wash or a shadow is MADE of.
/// Not black: a cool near-black, so a 9% wash over a cool surface stays on the
/// same side of neutral instead of going flat grey. Black over near-white is
/// the tone the whole of gh#177 is arguing with. The light-theme counterpart of
/// [`wash`]/[`white_alpha`] — callers that serve both themes go through
/// [`Theme::wash`] / [`Theme::white_alpha`], which apply
/// [`Theme::LIGHT_WASH_SCALE`] where it belongs.
pub fn ink(alpha: f32) -> Hsla {
    // The chroma is the same trace the surfaces carry, not more: ink is what a
    // NEUTRAL is made of here, and an idle dot or a normal meter paints it
    // straight — see [`spends_colour`], which those call sites assert against.
    cool(0.30, 0.012).opacity(alpha)
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

/// Does this tone spend COLOUR, or is it one of the theme's neutrals?
///
/// This is the question a dozen tests ask — of a quiet board state, of an idle
/// dot, of a normal meter, of a selected row (gh#173, gh#175). HSL saturation
/// stopped answering it when the light neutrals took on their trace of blue
/// (gh#177): `#f9fafc` is three points of channel spread and a saturation of
/// 0.33, because saturation is a ratio whose denominator collapses near white.
/// The spread is the honest measure, and the gap it straddles is not close —
/// no neutral in either theme spreads past 4% of the range, and the palest
/// thing that MEANS something is a status hue at ten times that.
pub fn spends_colour(color: Hsla) -> bool {
    let rgb = Rgba::from(color);
    let hi = rgb.r.max(rgb.g).max(rgb.b);
    let lo = rgb.r.min(rgb.g).min(rgb.b);
    hi - lo > 0.05
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
    /// tier's floor against BOTH reading panels — `bg` (the main panel) and
    /// `surface` (the shell) — so no screen can put readable copy below AA. The
    /// predecessor of this test was a hand-measured table in an issue, which is
    /// how eleven call-site multipliers landed two of the app's most common
    /// tones under 3:1.
    ///
    /// The raised surfaces are checked separately and against WCAG's own
    /// numbers, because only the top two tones are ever painted on them — see
    /// the second half. Measuring BOTH variants against BOTH is the gh#177
    /// half: the multipliers are gone, but the same token on a different
    /// surface is still a different number, and light's surfaces are no longer
    /// dark's read backwards.
    #[test]
    fn text_tones_clear_their_contrast_floors() {
        // (name, target on `bg`, floor that BOTH reading panels must clear)
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
            // The raised surfaces — a float, a ghost chip, your own message —
            // carry only the top two tones (a placeholder or a disabled control
            // never lands on one). Those two clear WCAG's own bars there:
            // AAA body for `text`, AA body for `text_muted`.
            for (surface, paint) in [
                ("card", t.card),
                ("surface_raised", t.surface_raised),
                ("bubble", t.bubble),
            ] {
                for (name, tone, floor) in [("text", t.text, 7.0), ("muted", t.text_muted, 4.5)] {
                    let ratio = contrast_ratio(tone, paint);
                    assert!(
                        ratio >= floor,
                        "{name} is {ratio:.2}:1 on {surface}, floor {floor} (light={})",
                        t.light
                    );
                }
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
                // state, and selection is not a state. (Measured as channel
                // spread rather than saturation: light's neutrals carry a
                // trace of blue and its washes are cool ink, so `s` reads a
                // third of full chroma on tones nobody would call blue —
                // gh#177, [`spends_colour`].)
                for c in [idle.rest, idle.hovered, picked.rest, picked.hovered] {
                    assert!(!spends_colour(c), "selection must spend no colour");
                }
                assert!(!spends_colour(picked.ring[0].color));
            }
        }
    }

    #[test]
    fn a_selected_row_inside_a_white_card_is_unmistakable() {
        // The subtle case (gh#175): the settings cards are white in light
        // mode, so the row steps DOWN and its hairline draws the well. The
        // bed is [`Theme::card`] — gh#177 split that off `surface`, which in
        // light is the shell BELOW the page rather than a card above it.
        let t = Theme::light();
        let picked = t.row(Bed::Card, true);
        let hovered = t.row(Bed::Card, false).hovered;
        let card = t.card;
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

    /// gh#177's first failure: elevation does not survive inversion. Dark
    /// elevates toward white and so does light — the ramp is not mirrored, it
    /// is re-declared, and a popover is never darker than the page it covers.
    #[test]
    fn light_elevation_runs_toward_white() {
        let t = Theme::light();
        // The ground is the shell, the paper is the main panel, and a float is
        // white. Inverted, this order ran backwards and a popover read as a
        // hole punched in the page.
        assert!(t.surface.l < t.bg.l, "the shell is the ground");
        assert!(t.bg.l < t.card.l, "a settings card is above the paper");
        assert!(t.bg.l < t.surface_raised.l, "and so is a float");
        assert!(t.surface_raised.l > 0.999, "the top of the ramp is white");
        // `surface` and `card` are the token gh#177 split: one number in dark,
        // where the shell IS the card tone, and two in light, where the shell
        // is below the page and a card is above it. A settings card painting
        // `surface` was the elevation defect wearing a different hat.
        let d = Theme::dark();
        assert_eq!(d.surface, d.card, "in dark the two coincide");
        assert_ne!(t.surface, t.card, "in light they cannot");
        // Text still walks the other way from the surfaces.
        assert!(t.surface_raised.l > t.text_faint.l);
        assert!(t.text_faint.l > t.text_subtle.l);
        assert!(t.text_subtle.l > t.text_muted.l);
        assert!(t.text_muted.l > t.text.l);
        // BOTH variants elevate toward their own bright end — that is the
        // structure the two share, rather than one being the other reversed.
        assert!(d.surface.l < d.surface_raised.l);
        assert!(t.surface.l < t.surface_raised.l);
        // Only white is achromatic. Every other light neutral leans blue, so
        // the white beside it does not read yellow — and it leans by a TRACE:
        // measured as channel spread (HSL saturation is meaningless this close
        // to white), no tone is more than 3% off achromatic.
        for c in [t.bg, t.surface].into_iter().chain(t.text_tones()) {
            let rgb = Rgba::from(c);
            assert!(!spends_colour(c), "a trace only, not a hue: {rgb:?}");
            assert!(rgb.b > rgb.r, "and the trace is BLUE: {rgb:?}");
            assert!(
                (0.55..0.75).contains(&c.h),
                "which puts the hue in the blues: h={}",
                c.h
            );
            assert_eq!(c.a, 1.0);
        }
        assert_eq!(t.surface_raised.s, 0.0, "white is white");
        // Dark stays monochrome — its greys were sampled off the original app.
        for c in [d.bg, d.surface, d.card, d.surface_raised]
            .into_iter()
            .chain(d.text_tones())
        {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
    }

    /// gh#177's third failure: a wash is not perceptually symmetric. Light's
    /// hairlines and washes are cool INK rather than black, and the washes run
    /// at a fraction of the alpha the call sites name.
    #[test]
    fn light_hairlines_and_washes_are_cool_ink() {
        let t = Theme::light();
        let is_ink = |c: Hsla| c.l > 0.1 && c.l < 0.4 && (0.55..0.75).contains(&c.h);
        for c in [t.border, t.border_strong] {
            assert!(is_ink(c), "hairlines are cool ink, got {c:?}");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        assert!(is_ink(t.element_hover), "the hover wash is cool ink");
        assert!(
            t.element_hover.a >= 0.05 && t.element_hover.a < 0.35,
            "alpha in band, got {}",
            t.element_hover.a
        );
        assert!(t.border.a < t.border_strong.a);
        // The primitives mirror the fields — and the wash scales while the
        // hairline does not: a wash floods a row, a hairline has to be seen.
        assert!(is_ink(t.wash(0.14)) && is_ink(t.white_alpha(0.09)));
        assert_eq!(t.white_alpha(0.09).a, 0.09);
        assert!(
            t.wash(0.14).a < 0.14,
            "black at the dark alphas is the scrim gh#177 is about"
        );
        // A fade must still rest on the theme's own zero, never on transparent
        // black, or the interpolation goes grey mid-way.
        assert_eq!(t.wash(0.0).a, 0.0);
        // Dark is untouched: white at exactly the named alpha.
        let d = Theme::dark();
        assert_eq!(d.wash(0.14), wash(0.14));
        assert_eq!(d.white_alpha(0.09), white_alpha(0.09));
        // A selected row on the light shell is the one wash that runs the
        // other way: it lifts to WHITE (gh#175), edged in ink so the lift
        // reads against the paper.
        let picked = t.row(Bed::Shell, true);
        assert_eq!(picked.rest.l, 1.0);
        assert!(is_ink(picked.ring[0].color), "and the edge is ink, not black");
    }

    /// gh#177's second failure: your own message. Inverted, the bubble painted
    /// `surface_raised` and landed a mid-grey slab in the middle of a white
    /// page — "mine" is a distinction, not an elevation.
    #[test]
    fn the_user_bubble_is_a_well_in_light_and_a_step_in_dark() {
        let t = Theme::light();
        // Not the raised surface, and not a slab: the shallowest step the paper
        // can take, carried by its hairline. The inverted #e4e4e4 sat at 0.89.
        assert_ne!(t.bubble, t.surface_raised);
        assert!(t.bubble.l < t.bg.l, "distinct from the page");
        assert!(t.bubble.l > 0.94, "and barely — the edge does the work");
        assert!(t.bubble_border.a > 0.0, "so there has to BE an edge");
        // Monochrome, like every other non-state surface: the hues mean state
        // (gh#173), and who said something is not a state.
        assert!(!spends_colour(t.bubble) && !spends_colour(t.bubble_border));
        // Dark keeps the tone it always had, and needs no hairline — the step
        // in tone carries the shape there.
        let d = Theme::dark();
        assert_eq!(d.bubble, neutral(0.235));
        assert_eq!(d.bubble_border.a, 0.0);
    }

    /// The macOS failure: light frost was plain white at 80% over the blurred
    /// desktop, so the wallpaper tinted the sidebar.
    #[test]
    fn the_light_frost_does_not_take_the_wallpapers_colour() {
        let t = Theme::light();
        let frost = t.glass();
        if Theme::GLASS_ALPHA < 1.0 {
            assert!(frost.s > 0.0, "the scrim has a tone of its own");
            assert!(
                frost.a >= Theme::GLASS_ALPHA,
                "and neutralises at least as hard as dark's: {} vs {}",
                frost.a,
                Theme::GLASS_ALPHA
            );
            // A float is white and near-opaque — it covers the page rather
            // than borrowing from whatever is behind the window.
            let card = t.float_card();
            assert!(card.l > 0.97 && card.a > 0.9, "float card {card:?}");
        }
    }

    /// A light float lifts with shadow, because it has nowhere brighter to go.
    #[test]
    fn light_floats_lift_with_shadow() {
        let (light, dark) = (Theme::light(), Theme::dark());
        // Dark keeps Tailwind's `shadow-lg` verbatim — its cards lift by tone.
        assert_eq!(dark.float_shadow().len(), 2);
        for s in dark.float_shadow() {
            assert_eq!(s.color, hsla(0.0, 0.0, 0.0, 0.1));
        }
        // Light's is stronger, cooler, and cast further: with `surface_raised`
        // at white there is no brighter tone left to elevate with.
        let cast = light.float_shadow();
        assert_eq!(cast.len(), 2);
        assert!(
            cast.iter().any(|s| s.color.a > 0.1),
            "light needs a shadow you can see"
        );
        for s in &cast {
            assert!(!s.inset, "a float casts outward");
            assert!(
                (0.55..0.75).contains(&s.color.h),
                "shade, not grime: h={}",
                s.color.h
            );
        }
        let widest = |shadows: Vec<gpui::BoxShadow>| {
            shadows
                .iter()
                .map(|s| f32::from(s.blur_radius))
                .fold(0.0, f32::max)
        };
        assert!(
            widest(cast) > widest(dark.float_shadow()),
            "and it casts further than dark's"
        );
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
