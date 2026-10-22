# Asset ledger

Every image, icon and font in the repository, with where it came from. Add a row when you add an asset; a file without a recorded origin doesn't get committed. Generated files name the script that makes them, and the script's sources have rows of their own. Dates are when the file entered the repository.

| File | What | Origin | Date |
|---|---|---|---|
| `brand/source/mark-reference.png` | Raster export of the logo design | Own work: designed for AudioSync in Adobe Illustrator by a member of the maintainer's family, contributed under the MIT License ([brand/README.md](../brand/README.md)) | 2026-10-17 |
| `brand/audiosync-mark.svg` | The logo mark | Own work: traced from `mark-reference.png` with potrace (`scripts/brand/trace-mark.sh`, `tidy-svg.py`) | 2026-10-17 |
| `brand/audiosync-icon.svg`, `brand/audiosync-icon-dark.svg` | App icon, light and dark | Own work: generated from the mark by `scripts/brand/compose.py` | 2026-10-17 |
| `brand/audiosync-wordmark.svg`, `brand/audiosync-wordmark-dark.svg` | Wordmark, light and dark | Own work: outlined from Poppins Medium by `scripts/brand/wordmark.swift` | 2026-10-17 |
| `brand/fonts/Poppins-Medium.ttf` | Wordmark typeface | Third-party: Poppins, © 2020 The Poppins Project Authors, SIL Open Font License 1.1 (`brand/fonts/OFL.txt`, listed in `THIRD_PARTY_NOTICES.md`) | 2026-10-17 |
| `brand/guide/source.svg`, `delay.svg`, `classes.svg` | The Guide's illustrations: the signal path from an app through AudioSync to several outputs, two speakers lined up by a delay, and outputs kept in step next to a Google Cast speaker playing after an offset set by ear | Own work: drawn in code by `scripts/brand/guide_art.py` (shapes and sine curves laid out by hand in the script, in colors from `brand/tokens.json`, with no reference image); the AudioSync tile carries the brand mark from `brand/audiosync-mark.svg`, scaled and rounded by the same script | 2026-10-22 |
| `brand/guide/*-dark.svg` | The same illustrations in dark appearance | Own work: generated from the light ones by `scripts/brand/guide_art.py` | 2026-10-22 |
| `icon-source.png` | App icon at 1024 px | Own work: rendered from `audiosync-icon.svg` by `scripts/brand/render-svg.swift` | 2026-10-17 |
| `AppIcon.icns` | The Mac app icon | Own work: built from `icon-source.png` by `makeicon.sh` | 2026-10-17 |
| `site/favicon.ico`, `site/favicon.png`, `site/favicon-16x16.png`, `site/favicon-32x32.png`, `site/apple-touch-icon.png`, `site/mstile-150x150.png`, `site/icon.png`, `site/icon-192.png`, `site/icon-512.png` | Site icons | Own work: built from `icon-source.png` by `make-web-icons.sh` | 2026-10-17 |
| `site/wordmark.svg`, `site/wordmark-dark.svg` | Site wordmark | Own work: copies of the brand wordmarks (`scripts/brand/build-brand.sh`) | 2026-10-17, 2026-10-19 |
| `site/apple-touch-icon.png`, `site/icon-512.png`, built into the web remote by `remote/src/page.rs` | Web remote icons, placeholders until the remote gets its own | Own work: the site icons above, served as the page's `/icon-180.png` and `/icon-512.png` | 2026-10-22 |
| `windows/res/audiosync.ico`, `windows/res/audiosync-256.png` | Windows app icon | Own work: from `audiosync-icon.svg` by `scripts/brand/windows-icons.sh` | 2026-10-18 |
| `windows/res/tray-light.ico`, `tray-dark.ico`, `tray-light-off.ico`, `tray-dark-off.ico` | Windows tray icons | Own work: from the mark in the token colors by `scripts/brand/windows-icons.sh` | 2026-10-18 |
| `docs/images/main-window-light.png`, `main-window-dark.png`, `menu-bar-panel-light.png`, `menu-bar-panel-dark.png`, `equalizer-light.png`, `equalizer-dark.png` | App screenshots | Own work: the app's own views rendered from mock data by `scripts/screenshots/render.sh` | 2026-10-17 |
| `docs/images/social-preview.png` | Link preview image | Own work: composed from the brand files and the main window screenshot by `scripts/screenshots/social-preview.swift` | 2026-10-17 |
