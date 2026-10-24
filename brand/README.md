# AudioSync brand

The logo is a lowercase "a" whose bowl carries a sound wave. Every other file here is
generated from the traced mark, `audiosync-mark.svg`, and the Poppins Medium font
in `fonts/`.

## The mark

**Idea.** The lowercase "a" is for audio, and for the one Mac it starts from.
Its bowl is a loop that the wave breaks open on the left: one stream that goes around and comes back in step,
the way every speaker plays the same moment. The wave runs through the loop
instead of sitting inside it, so the sound isn't held in one place; it passes
through and carries on to the next room. The stem stands open on the right,
the side the sound leaves from. Teal sits between the blue of a cable and the
green of a room full of plants: technical, but calm.

**Origin.** The logo was designed for AudioSync in Adobe Illustrator by a member
of the maintainer's family, who contributed it to the project under the MIT
License, which lets it ship in this GPL-3.0-or-later project. `source/mark-reference.png` is a
raster export of that design. The vector mark, `audiosync-mark.svg`, was traced
from it with potrace through `scripts/brand/trace-mark.sh` (the steps are
described below), and every other file here is generated from the mark and the
Poppins font.

| File | Use |
|---|---|
| `audiosync-mark.svg` | The mark on its own (brand teal, tight bounds) |
| `audiosync-icon.svg` | App icon, light (macOS icon grid: 824 pt tile on a 1024 pt canvas) |
| `audiosync-icon-dark.svg` | App icon, dark variant |
| `audiosync-wordmark.svg` | Wordmark for light backgrounds |
| `audiosync-wordmark-dark.svg` | Wordmark for dark backgrounds |
| `fonts/Poppins-Medium.ttf`, `fonts/OFL.txt` | Wordmark typeface and its license (SIL OFL 1.1) |
| `source/mark-reference.png` | Raster export of the original design, which the mark was traced from |

## Colors

| Name | Hex | Used for |
|---|---|---|
| Teal | `#1C9F91` | Mark, "sync", app accent |
| Mint | `#63DAC2` | Mark and "sync" on dark backgrounds |
| Off-white | `#F6F6F6` | "audio" on dark backgrounds |
| Charcoal | `#292929` | "audio" on light backgrounds |

## Design tokens

`tokens.json` holds the colors (light and dark), the typeface, spacing and corner
radii that the Mac app, the Windows app and the website share. Edit it, then run

```bash
python3 -I scripts/brand/tokens.py
```

which writes `Sources/AudioSync/BrandTokens.swift` (the Mac app's `Brand` constants
read it), `desktop/ui/tokens.slint` and `desktop/src/tokens.rs` (the Windows app and
its contrast test), and the CSS variables between the `tokens:begin` and
`tokens:end` comments in `site/index.html`. CI runs it and fails when a generated
file differs. The contrast pairs at the end of the file are the text colors on the
backgrounds the Windows app draws them on; its tests fail below 4.5:1.

## Typeface

The wordmark is set in [Poppins](https://github.com/itfoundry/Poppins) Medium with
-0.02 em tracking: "audio" in charcoal (or off-white), "sync" in teal (or mint).
The SVG wordmarks are outlined, so they need no font; the app bundles the font and
renders the wordmark as text (`Sources/AudioSync/Brand.swift`).

In the app, the headers of the main window and the menu bar panel show the mark
and the wordmark (`Sources/AudioSync/MainWindow.swift`, `MenuBar.swift`), and the
menu bar icon is the mark as a template image, dimmed while streaming is off
(`Sources/AudioSync/MenuBar.swift`).

## Regenerating

```bash
scripts/brand/build-brand.sh   # icons, wordmarks, SwiftUI mark, AppIcon.icns, site icons and wordmark
scripts/brand/trace-mark.sh    # re-trace the mark from source/ (needs: brew install potrace)
scripts/brand/windows-icons.sh # the Windows app and tray icons in desktop/res/
python3 -I scripts/brand/guide_art.py # the Guide's illustrations in guide/, light and dark, and GuideArtwork.swift
```

`build-brand.sh` writes `audiosync-icon*.svg`, `audiosync-wordmark*.svg`,
`Sources/AudioSync/BrandMark.swift`, `icon-source.png`, `AppIcon.icns`, the icons in
`site/`, `site/wordmark.svg` and `site/wordmark-dark.svg`. Edit the sources and scripts, not the generated
files. It calls `makeicon.sh` (`AppIcon.icns`, from `icon-source.png` with `sips`
and `iconutil`) and `make-web-icons.sh` (the `site/` icons, with `sips`); both take
an optional source PNG and output path. `scripts/brand/check-icons.sh`, run by CI,
checks that the two scripts still reproduce the committed icons.

The mark was vectorized with potrace (`scripts/brand/trace-mark.sh`): the reference
is upsampled 4x, lightly smoothed and thresholded, traced with `-a 1.0 -O 1`, then
tidied (`scripts/brand/tidy-svg.py`: curves that are straight within 0.35 px become
lines, near-axis lines are snapped to the axis). The result is two shapes with 67
segments that match the reference to 99.5% (intersection over union). vtracer was
compared on the same input; its cleanest settings needed about 170 segments and fit
less closely.
