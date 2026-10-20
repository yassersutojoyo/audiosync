# Contributing

Issues, reports of device combinations that work (or don't), and pull requests are all welcome. AudioSync is a small side project, so please be patient with replies.

[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) covers building, testing and the layout of the code. The short version: `./setup.sh` once, then `./build.sh` and `Tests/run-tests.sh`.

## Before you open a PR

CI builds the Rust audio core and the app (`scripts/build_core.sh`, then `swift build -c release`) and checks that the core is linked, builds the whole app (`./build.sh --no-install`) and checks the bundle and its signatures (`Tests/check-app-bundle.sh`), runs `Tests/run-tests.sh`, runs `git diff --check` over the whole tree, compiles the Python scripts, checks that the generated brand SVGs, `BrandMark.swift` and the design-token outputs of `scripts/brand/tokens.py` (`BrandTokens.swift`, the Windows app's `tokens.slint` and `tokens.rs`, and the site's CSS variables) are up to date, runs `shellcheck` on every shell script in `scripts/shellcheck-files.txt`, runs `scripts/privacy_check.py` (no home-directory paths from a Mac or a Windows PC, LAN hostnames or private IP addresses in tracked files; its forbidden-names guard is described in `docs/DEVELOPMENT.md`). The Core, AirPlay and Windows workflows also run `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and `cargo deny check licenses` in `core/`, `airplay/` and `windows/`. Please make sure those pass. `Tests/run-tests.sh` runs the same shellcheck check, so install shellcheck 0.11.0 first (`brew install shellcheck`, or see `docs/DEVELOPMENT.md`). If you touch the audio path, also run `Tests/run-tests.sh --hardware` locally.

## Style

- Match the code around you: naming, comment density, idioms.
- Real-time code (render and IOProc callbacks) must not allocate, lock or do I/O.
- User-facing strings go in `AppText` (`Localization.swift`).
- Code, comments, docs and commit messages are in English. No trailing whitespace.
- Don't edit generated brand files by hand; see [`brand/README.md`](brand/README.md).

## Scope

AudioSync tries to stay focused: capture one source, play it in sync on local and AirPlay speakers, with AudioSync's own AirPlay sender built in. Big new dependencies or features outside that are unlikely to be merged, so open an issue first if you're planning one.

A new dependency needs a license that fits and an entry in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Please don't copy code from other projects into AudioSync.
