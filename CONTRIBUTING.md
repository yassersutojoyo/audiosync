# Contributing

Issues, reports of device combinations that work (or don't), and pull requests are all welcome. AudioSync is a small side project, so please be patient with replies.

[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) covers building, testing and the layout of the code. The short version: `./setup.sh` once, then `./build.sh` and `Tests/run-tests.sh`.

## Before you open a PR

CI builds the Rust audio core and the app (`scripts/build_core.sh`, then `swift build -c release`) and checks that the core is linked, runs `Tests/run-tests.sh`, runs `git diff --check` over the whole tree, compiles the Python scripts, checks that the generated brand SVGs and `BrandMark.swift` are up to date, runs `shellcheck` on every shell script in `scripts/shellcheck-files.txt`, and runs `./setup.sh --patches-only <dir>`, which downloads the pinned OwnTone source and fails if any of AudioSync's engine patches no longer applies. The Core and Windows workflows also run `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and `cargo deny check licenses` in `core/` and `windows/`. Please make sure those pass. `Tests/run-tests.sh` runs the same shellcheck check, so install shellcheck 0.11.0 first (`brew install shellcheck`, or see `docs/DEVELOPMENT.md`). If you touch the audio path, also run `Tests/run-tests.sh --hardware` locally.

## Style

- Match the code around you: naming, comment density, idioms.
- Real-time code (render and IOProc callbacks) must not allocate, lock or do I/O.
- User-facing strings go in `AppText` (`Localization.swift`).
- Code, comments, docs and commit messages are in English. No trailing whitespace.
- Don't edit generated brand files by hand; see [`brand/README.md`](brand/README.md).

## Scope

AudioSync tries to stay focused: capture one source, play it in sync on local and AirPlay speakers, and keep the AirPlay engine self-contained. Big new dependencies or features outside that are unlikely to be merged, so open an issue first if you're planning one.

The AirPlay engine (OwnTone) is GPL and runs as a separate process. AudioSync only talks to it through the config file it writes, command-line arguments, signals, the local HTTP API and the audio pipe. Changes to the engine itself go in as patches in `setup.sh`, each listed in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Please don't copy code from other projects into AudioSync.
