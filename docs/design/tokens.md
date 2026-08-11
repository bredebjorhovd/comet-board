# Tokens: canvas → theme

Every variable the design canvases declare, and the `crates/ui/src/theme.rs`
field that paints it. Reconciled for gh#274.

The canvases are in `canvas/`. All five declare the same table, so this is one
palette, not five — see `canvas/README.md`.

Locked by `every_canvas_token_is_the_value_the_canvas_declares` in `theme.rs`.
Change a value here and that test fails, which is the point: the last redesign
pass drifted because nothing compared the two.

## Surfaces

| Canvas | Dark | Light | Theme field |
| --- | --- | --- | --- |
| `--card` | `#070707` | `#ffffff` | `bg` (`DARK_PANEL` / `LIGHT_PANEL`) |
| `--shell` | `#0d0d0d` | `#e9e9ec` | `surface` (`DARK_SHELL` / `LIGHT_SHELL`) |
| `--raised` | `#161616` | `#f4f4f7` | `surface_raised`, `card`, `bubble` |
| `--sel` | `#191919` | `#ffffff` | `row_selected` |
| `--selcard` | `#191919` | `#eeeef2` | `row_selected_card` |
| `--hover` | `rgba(255,255,255,.05)` | `rgba(0,0,0,.025)` | `element_hover` |
| `--chip` | `rgba(255,255,255,.07)` | `rgba(0,0,0,.05)` | `chip` |
| `--line` | `rgba(255,255,255,.08)` | `#e4e4e8` | `border` |
| `--line2` | `rgba(255,255,255,.13)` | `#d6d6dc` | `border_strong` |

Note the canvas's `--card` is the **main panel**, and the theme's `card` field
is the **raised inner card**. The names cross. `bg` is the one that means
`--card`; `card` and `surface_raised` are both `--raised`.

## Text

Four tones, never multiplied.

| Canvas | Dark | Light | Theme field |
| --- | --- | --- | --- |
| `--text` | `#ededed` | `#171717` | `text` |
| `--muted` | `#a8a8a8` | `#545454` | `text_muted` |
| `--subtle` | `#808080` | `#6b6b6b` | `text_subtle` |
| `--faint` | `#666666` | `#8a8a8a` | `text_faint` |

## Status ramp

One lightness, one chroma, four hues — `oklch(L C H)`.

| Canvas | Dark | Light | Theme field |
| --- | --- | --- | --- |
| `--blocked` | `oklch(0.74 0.14 25)` | `oklch(0.52 0.16 25)` | `danger` |
| `--working` | `oklch(0.74 0.14 75)` | `oklch(0.52 0.16 75)` | `warning` |
| `--review` | `oklch(0.74 0.14 265)` | `oklch(0.52 0.16 265)` | `accent` |
| `--settled` | `oklch(0.74 0.14 160)` | `oklch(0.52 0.16 160)` | `settled` |

Canvases tint with these via `color-mix(in srgb, var(--x) 12%, transparent)` for
a chip fill and `32%` for a warning border. Those percentages are part of the
spec; the surface issues wire them.

## Shadow and lift

| Canvas | Dark | Light | Theme |
| --- | --- | --- | --- |
| `--lift` | `none` | `0 1px 2px rgba(0,0,0,.05)` | `float_card` |
| `--sellift` | `0 0 0 1px rgba(255,255,255,.13)` | `0 0 0 1px #dcdce2, 0 1px 2px rgba(0,0,0,.06)` | `row_edge` (+ `LIGHT_SELECT_EDGE`) |
| `--cardshadow` | `none` | `0 1px 2px rgba(0,0,0,.04), 0 10px 30px -18px rgba(0,0,0,.18)` | `float_card` |

Dark lifts with tone and a ring; light lifts with shadow. A selected row's ring
is painted as an INSET shadow (`hairline_ring`) — a drop shadow behind a
translucent fill shows through as a plate.

## Borrowed marks

| Canvas | Dark | Light | Theme field |
| --- | --- | --- | --- |
| `--claude` | `#d97757` | `#c15f3c` | `claude` |

Outside the status ramp on purpose: it identifies a vendor, and the four hues
mean state. Nothing but the Claude mark paints with it.

## Not app tokens

`--desk` (`#17191b` / `#c3c6ca`) is the fake desktop the canvas draws *behind*
the window so the corner radius and shadow read. It has no counterpart in the
app and must not acquire one.

## What was wrong (gh#274)

Everything above matched already **except**:

1. **The four dark greys were generated, not transcribed.** They came from a
   `neutral(L)` oklch ramp, which landed under the design's numbers every time:
   `#eaeaea` for `#ededed` (three points, and on a title that shows), `#a7a7a7`
   for `#a8a8a8`, `#7f7f7f` for `#808080`, `#656565` for `#666666`. The light
   greys were hex constants and were exact — so this was one side of one
   variant, and re-deriving is what caused it. Both sides are now transcribed.
2. **`--chip` had no token.** Chips were landing on `surface_raised`, an opaque
   `#161616`. The canvas draws a translucent 7% wash, which composites
   differently on the card, on `--raised`, and inside a selected row — three
   beds where an opaque tone is right on at most one.
3. **`--claude` had no token.**

Surfaces, hairlines, selection, the light greys and the whole status ramp were
already exact.

## Deliberate deviations

One, and it predates this pass:

- **`--hover` in dark** is soft-white (L 0.92) at 5%, not pure white at 5%. A
  hover over the glass sidebar rested on pure white and flashed dark mid-fade
  (user reports). Same alpha, same neutral, one step off pure. Asserted in the
  token test so it stays a decision rather than becoming a drift.

Anything else that differs from the table above is a bug, not a deviation. Add
to this list only with the reason, and assert the deviating value.
