# Settings window — the canvas, as checkable claims

Source: `canvas/comet-settings-window.dc.html`. Tokens: `tokens.md`. Shell
claims it shares with the main window: `window.md`. Issue: gh#277.

Every claim is a thing you can look at one of and say yes or no. Written from
the canvas markup before the app was opened, so the list is not a description of
what we already do.

Numbers are canvas pixels at 1320×880. `--x` names a token; a claim that says a
token means the field in `Theme`, never a literal at the call site.

The canvas draws the settings route with **Agents** selected, so its right half
is the Accounts page in full. Claims A–C are the settings *shell* and hold for
every section; D–I are Accounts; J is the grammar the other seven pages inherit
from the same markup.

**State:** every claim below is satisfied as of gh#277 except the five listed
under *Deliberate deviations*, each with its reason. Ground truth:
`screenshots/gh277-*-{before,after}.png`, dark and light, captured at 1320×880
on an isolated engine (`COMET_IPC_PORT`, scratch data dir).

## A. Window and titlebar

- **A1** The window is the one `window.md` A describes: 1320×880, radius 12,
  `--shell` ground, a 1px `--line` divider at x=256 running the full height.
- **A2** The titlebar is 38px with the same left cluster as `window.md` B1/B2 —
  78px of traffic lights, then sidebar-toggle, back and forward at 24px, back
  `--subtle` and forward `--faint`.
- **A3** Settings draws **no tab strip**. In its place, one word — "Settings",
  13px/500 `--muted` — starting at the same x=172 the tab strip starts at.

## B. Settings nav (256px, 8px side padding)

- **B1** Header row: "Settings" 11px/600 `--subtle`, padding 10/9/6.
- **B2** The section rows are 3px apart, radius 10, padding 6×9, 13px.
- **B3** A row is a 16px icon then the label, 9px apart.
- **B4** An unselected row is `--muted` copy with a `--subtle` icon.
- **B5** The selected row paints `--sel` + the `--sellift` ring, its copy goes
  `--text` at weight 500, **and its icon goes `--text` too** — the icon moves
  with the label, not one tone behind it.
- **B6** The order is Devices, Agents, Members, Appearance, Shortcuts, Routing,
  Automations, Stats, Archived. (Automations arrived with gh#490, placed with
  the board-hosted pair it belongs to: after Routing, before Stats.)
- **B7** The nav ends at Back: a flex spacer, a 1px `--line` divider inset 8px
  inside the nav's own 8px gutter, then the row.
- **B8** Back is the same row shape at a 7px gap — a 16px chevron-left
  (`AltArrowLeft`, not the straight history arrow) in `--subtle`, "Back" in
  `--muted` — with 8px margin above and below.
- **B9** There is **no account footer**. The column ends at Back.

## C. The page card

- **C1** The section fills the rest: `--card` bed, 1px `--line`, radius 14,
  `--cardshadow`, margin 0/8/8/8 — the same inset card every route draws.
- **C2** Inside it the content is centred with padding 26/24/0.
- **C3** The content column is `width:100%; max-width:768` — the gutters are
  outside the measure, not eaten from it.
- **C4** Blocks in that column are **18px** apart.

## D. Accounts — the header block

- **D1** The header block is a column with **4px** between its two rows.
- **D2** Title row, 10px gaps, vertically centred: "Accounts" 15px/600 at
  letter-spacing -.01em `--text`, then the account count 13px `--subtle`, then a
  spacer, then two actions.
- **D3** The first action is the **device switcher**, and it is a standing chip:
  26px tall, radius 6, padding 0×9, `--chip` bed, 7px gaps — a 13px `--subtle`
  device glyph, the device name 12px `--muted`, a 12px chevron-down.
- **D4** The second action is **Refresh**: the word alone, 12px `--muted`, in a
  26px slot of the same radius and padding as the chip but with no bed and no
  glyph.
- **D5** Subject before verb: the switcher sits left of Refresh.
- **D6** The subtitle is 13px/20 `--subtle`, max-width 560: "The Claude Code,
  Codex and OpenCode logins on this device. Comet spends whichever one a route
  names."

## E. Accounts — a provider section

- **E1** A provider section is a column with **8px** between its header and its
  card.
- **E2** The header row is padding 0×2, 10px gaps: the brand mark at 14px in
  `--claude`, the provider name 13px/600 `--text`, a spacer, then the action.
- **E3** The action is "Add account": 12px `--subtle` copy behind a 14px
  plus-circle at a 6px gap, with **no bed and no padding** — it aligns with the
  card's right edge, not inset from it.
- **E4** The card is `--raised`, 1px `--line`, radius 14, clipped.
- **E5** A provider whose notice applies draws it **between the header and the
  card**, not inside it.

## F. Accounts — an account row

- **F1** A row is padding 14×18, 12px gaps. Rows after the first carry a 1px
  `--line` top border and nothing else — no fill of their own.
- **F2** The avatar is a 30px circle: 1px `--line2` ring, **no fill**, the
  initial 12px/500 `--muted`.
- **F3** A row with meters aligns its avatar to the TOP; a row without them
  centres it.
- **F4** The body column puts **9px** between the title line and each meter, and
  between the meters.
- **F5** Title line, 8px gaps: the address 13px/500, then the badges.
- **F6** The **active** account's address is `--text`; every other address is
  `--muted`.
- **F7** The Active badge is padding 1×8, radius 6, 11px, `--settled` copy on a
  `--settled` fill at 14%.
- **F8** The plan badge is padding 1×8, radius 6, 11px, `--subtle` copy inside a
  1px `--line` hairline — no fill.
- **F9** Actions appear only on an inactive account, right-anchored, 6px apart.
- **F10** Forget is icon-only: a 26×26 slot, radius 6, a 14px trash in
  `--subtle`.
- **F11** Switch is the page's one filled button: 26px tall, radius 6, padding
  0×10, `--text` bed with `--card` copy at 12px/500.

## G. Accounts — a usage meter

- **G1** A meter is one 10px-gapped row at 12px `--subtle`: label, bar, percent,
  reset.
- **G2** The label column is 42px wide.
- **G3** The bar is `flex:1`, max-width 240, 5px tall, radius 3.
- **G4** The track is `--text` at 8%; a normal fill is `--text` at 42%.
- **G5** A window at warning level paints its fill AND its percent in
  `--working`.
- **G6** The percent column is 56px wide.
- **G7** The reset time is `--faint`, and it is the only `--faint` thing on the
  row.

## H. Accounts — the notice strip

- **H1** The strip is radius **10** (not the card's 14), padding 10×14, 9px gap,
  aligned to the top of its glyph.
- **H2** It is bordered in `--working` at 30% over a `--working` fill at 9%,
  with 12px/18 copy in `--working` and a 14px triangle glyph.
- **H3** A command inside the copy is mono at 11.5px and stays in the strip's
  own tone — a code span borrows no status hue of its own.

## I. Accounts — the Rates card

- **I1** A "Rates" section closes the page: header 13px/600 `--text`, a spacer,
  then "USD per million tokens" 12px `--subtle`.
- **I2** Its card is the same `--raised`/`--line`/14 card, opening with a column
  header at padding 10×18, 11px `--faint`, `--line` bottom border: "Model"
  flexible, then Input, Output, Cache rd, Cache wr right-aligned at 64px each.
- **I3** A model row is padding 9×18, 12px `--muted`, the four figures mono and
  right-aligned in the same 64px columns, `--line` above each row after the
  first.
- **I4** A "Your seats" row follows at padding 11×18: the label 12px `--muted`,
  the total 12px mono `--text`.
- **I5** The card ends with a footnote at padding 10×18: a 13px `--faint` info
  glyph, then 12px `--subtle` copy naming Board stats in `--muted`.

## J. The grammar the other seven pages inherit

The canvas draws one page, but every widget in it is a shared one, so the same
claims are checkable on Devices, Members, Appearance, Shortcuts, Routing, Stats
and Archived.

- **J1** Page title 15px/600 `--text`, count 13px `--subtle` beside it,
  subtitle 13px/20 `--subtle` at max-width 560, 4px under the title.
- **J2** Section card: `--raised`, 1px `--line`, radius 14, clipped, 8px under
  its header, 18px from the block before it.
- **J3** Row: padding 14×18, `--line` top border after the first, no fill.
- **J4** In light mode a settings card is already the raised object on the page,
  so a row inside it **dims** rather than lifts: the selected row takes
  `--selcard` (`Bed::Card`), never `--sel`. In dark both land on the same tone.
- **J5** Every tone above comes from `Theme`. The canvas's `--chip`, `--claude`,
  `--line2` and the status ramp all have fields (`tokens.md`); a call site that
  re-derives one by hand is a defect even when the number matches.

## K. Both themes

- **K1** Every claim above holds in light with the light value of each token.
- **K2** In dark `--lift` and `--cardshadow` are `none`; in light they carry
  their shadows. That is the only structural difference between the variants.

## Deliberate deviations

- **G4's two alphas are painted with `Theme::white_alpha`, not with `--text`.**
  The canvas mixes the reading tone (`color-mix(--text 8%/42%)`); the app paints
  the crate's unscaled fill primitive at the same two alphas — soft-white over
  dark, ink over light. The rule it obeys is `crates/ui/tests/text_tones.rs`:
  a text tone is never multiplied by an alpha, and a bar is a fill. The two land
  within four channel steps of each other on this bed, in both themes, and the
  substance of the claim — one neutral at two alphas, no colour until a window
  earns it — is exact.

- **H2's copy is `warning_text()`, not `--working` itself.** The canvas sets the
  strip's sentence in the same hue as its border. The app keeps amber-200 in
  dark and amber-700 in light (gh#178): the ramp hue at `oklch(0.52 …)` on a 9%
  wash of itself is the least legible any text in the app got, on the one widget
  whose whole job is to be read. Border and fill are the canvas's exactly.

- **H3's mono span does not land.** A notice's sentence arrives from the engine
  as one string (`AgentAccountWarning::message`); there is no markup in it to
  set a command in mono. The strip paints the whole sentence in its own tone,
  which is the half of H3 that was about not borrowing a status hue.

- **E2 draws no mark for Codex, and the app draws one.** The canvas gives
  Claude Code a `--claude` brand mark and leaves the Codex header as bare text.
  The app marks every provider (OpenAI, Cursor, OpenCode) in `--muted`, because
  the sections are told apart by their marks and an unmarked one reads as a
  continuation of the section above it. The Claude mark keeps `--claude`; no
  other provider borrows a hue.

- **I1–I5, the Rates card, is not on this page.** Both halves of it already
  exist, deliberately, elsewhere:

  - The per-model table went to Board stats in gh#252, where it is the
    breakdown's model cut and sits beside the spend it prices, with its own
    provenance line ("built-in table of {date}, overridden by routing.toml").
    Re-drawing it here would be a second copy of the same numbers on a page
    about *logins*, and the copy without the spend is the less useful one.
  - "Your seats" is per-account here (gh#178): the plan chip on each row is that
    login's monthly cost, written to the board's `routing.toml` from the row
    that spends it. A page that showed only the total would be a page you cannot
    edit the parts on.

  The footnote's sentence — the rates only price Board stats, and Comet never
  sees your bill — is on the stats page, next to the figure it qualifies.

  If the total is wanted here, it is one row's worth of work over `Plans`; it is
  left out rather than guessed at.
