# Contributing

Issues, reports of device combinations that work (or don't), and pull requests are all welcome. AudioSync is a small side project, so please be patient with replies.

[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) covers building, testing and the layout of the code. The short version: `./setup.sh` once, then `./build.sh` and `Tests/run-tests.sh`.

## Before you open a PR

CI builds the Rust audio core and the app (`scripts/build_core.sh`, then `swift build -c release`) and checks that the core is linked, builds the whole app (`./build.sh --no-install`) and checks the bundle and its signatures (`Tests/check-app-bundle.sh`), runs `Tests/run-tests.sh`, runs `git diff --check` over the whole tree, compiles the Python scripts, checks that the generated brand SVGs, `BrandMark.swift` and the design-token outputs of `scripts/brand/tokens.py` (`BrandTokens.swift`, the Windows app's `tokens.slint` and `tokens.rs`, and the site's CSS variables) are up to date, runs `shellcheck` on every shell script in `scripts/shellcheck-files.txt`, runs `scripts/privacy_check.py` (no home-directory paths from a Mac or a Windows PC, LAN hostnames or private IP addresses in tracked files; its forbidden-names guard is described in `docs/DEVELOPMENT.md`). The Core, AirPlay and Windows workflows also run `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and `cargo deny check licenses` in `core/`, `airplay/` and `windows/`. Please make sure those pass. `Tests/run-tests.sh` runs the same shellcheck check, so install shellcheck 0.11.0 first (`brew install shellcheck`, or see `docs/DEVELOPMENT.md`). If you touch the audio path, also run `Tests/run-tests.sh --hardware` locally.

## Style

- Match the code around you: naming, comment density, idioms.
- Real-time code (render and IOProc callbacks) must not allocate, lock or do I/O.
- User-facing strings go in `UIStrings` (`Localization.swift`).
- Code, comments, docs and commit messages are in English. No trailing whitespace.
- Don't edit generated brand files by hand; see [`brand/README.md`](brand/README.md).

## Scope

AudioSync tries to stay focused: capture one source, play it in sync on local and AirPlay speakers, with AudioSync's own AirPlay sender built in. Big new dependencies or features outside that are unlikely to be merged, so open an issue first if you're planning one.

A new dependency needs a license that fits and an entry in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Please don't copy code from other projects into AudioSync.

## Contributor terms

The short version: you keep the copyright on what you send, and AudioSync stays GPL-3.0-or-later in public. You also let me, the maintainer, use your contribution under other licenses, which keeps the door open for a paid edition later. If that doesn't sit right with you, open an issue instead of a pull request and we can talk it through.

To agree, put this line in your pull request description: "I agree to the contributor terms in CONTRIBUTING.md". I check for it before merging. No sign-off lines or separate forms are needed.

1. A "contribution" is anything you submit to this repository to be included in AudioSync: code, tests, docs, artwork, translations and so on. The "maintainer" is Yasser Sutojoyo Kusumo, or a person or organization the maintainer transfers the project to in writing.
2. You keep the copyright in your contribution. These terms don't transfer ownership of anything.
3. You give the maintainer permission to use, copy, modify, distribute and sublicense your contribution, on its own or as part of other work, under any license the maintainer chooses, proprietary licenses included. This permission is worldwide, non-exclusive, free of charge, perpetual and irrevocable.
4. If you hold a patent that your contribution would infringe, on its own or combined with AudioSync as it stood when you submitted it, you give the maintainer and everyone who receives AudioSync a license under that patent to make, use, sell, import and distribute the contribution, on the same conditions as point 3.
5. In return, every contribution that gets merged stays licensed to the public under GPL-3.0-or-later. Any other license the maintainer uses comes in addition to that one, never in place of it.
6. You confirm you have the right to agree to this: the contribution is your own work, or your employer (or anyone else with rights in it) has allowed you to submit it on these terms.
7. You confirm the contribution contains no code or other material copied from another project, unless you mark it clearly in the pull request with where it came from and its license. Material like that stays under its own license and isn't covered by point 3.
8. Apart from what you confirm in points 6 and 7, your contribution comes as is, without any warranty, and you aren't expected to support it.
9. The maintainer may update these terms. A new version only applies to contributions submitted after it is published here; earlier contributions stay under the version that applied when they were made.

This isn't legal advice, and I'm not a lawyer. If your situation is unusual, for example your employer has rules about open-source work, please check before you submit.
