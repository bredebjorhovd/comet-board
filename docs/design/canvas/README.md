# Design canvases

These five files are the **visual source of truth** for Comet. When the app and
a canvas disagree, the app is wrong.

| File | Surface | Issue |
| --- | --- | --- |
| `comet-window.dc.html` | Main window — sidebar, chat, shell chrome | gh#275 |
| `comet-review-window.dc.html` | Review — claims, effects, unclaimed changes | gh#276 |
| `comet-settings-window.dc.html` | Settings, all sections | gh#277 |
| `comet-stats-window.dc.html` | Stats page | gh#278 |
| `comet-ios.dc.html` | iOS app | gh#279 |

They are a verbatim export of the "App design improvement review" project on
claude.ai/design.
An agent cannot reach that project, which is why they are vendored here — gh#258
pointed at `/Users/brede/Downloads/*.dc.html` and no worktree could read it.

## Do not edit them to match the code

The point of a source of truth is that it does not move when the thing it
describes is wrong. If a canvas is genuinely out of date, re-export it from
claude.ai/design and replace the file wholesale in its own commit, so the diff
shows a design change rather than a concession.

## Opening one

They are self-contained apart from two relative references, both satisfied here:

- `./support.js` — the `x-dc` runtime, vendored alongside them.
- `crates/ui/assets/fonts/*.ttf` — reached through `crates/ui/assets/fonts`
  here, a symlink to the real font directory. Without Geist the layout still
  holds but the metrics shift, so judge spacing only with the fonts loading.

  The symlink points at the **fonts directory alone**, deliberately. A symlink
  to `crates/` was the obvious version and it broke `env_isolation`: the source
  scanners walk `docs/` too, followed the link, and found every `.rs` file in
  the repo a second time under this path. Keep it narrow, or keep that failure.

Each canvas takes a `theme` prop of `dark` or `light`, defaulting to dark, and
declares its own preview size — 1320×880 for the desktop surfaces, which is the
size the real window opens at. Screenshot the app at that size or the comparison
is not one.

## What they agree on

All five declare the same palette. The three desktop canvases are byte-identical
in their `.cw` blocks; review drops `--desk`; iOS drops the four that only exist
because a desktop window sits on a desk (`--shell`, `--hover`, `--cardshadow`,
`--desk`). There is one design here, not five — see `../tokens.md`.
