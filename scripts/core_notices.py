#!/usr/bin/env python3
"""Write the notices of the Rust crates in the macOS app's audio core.

    python3 -I scripts/core_notices.py --output <file> <target triple>...

The macOS app links one Rust library, audiosync-core (core/), and through it
AudioSync's own AirPlay and Google Cast senders and web remote (airplay/,
cast/, remote/) with their third-party crates. This writes one text file covering every crate compiled into the core
for the given targets (scripts/build_core.sh passes both Mac targets): first
AudioSync's own packages and license, then each crate with its version,
license expression and repository and the full text of every license or
notice file in its package (LICENSE*, LICENCE*, COPYING*, NOTICE*,
COPYRIGHT*), or its license's standard text with the crate's authors when the
package has no such file. Identical texts are written once and referenced
afterwards. Build and development dependencies and procedural macros aren't
part of the library and are left out. The Rust standard library's own notices
are copied next to this file by scripts/build_core.sh.

It reuses scripts/windows_notices.py, pointed at core/ and core/deny.toml, so
the checks are the same: it fails when a crate's license isn't allowed by
core/deny.toml, or when a crate's sources point to a license file the package
doesn't ship and no supplement covers it. It needs only cargo and the Python
standard library.
"""
import argparse
import importlib.util
import os
import re
import sys

SCRIPTS = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(SCRIPTS)
CORE = os.path.join(REPO, "core")
ROOT_PACKAGE = "audiosync-core"


def shared():
    """scripts/windows_notices.py as a module, working in core/."""
    spec = importlib.util.spec_from_file_location(
        "windows_notices", os.path.join(SCRIPTS, "windows_notices.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # Its functions read these at call time: cargo runs in core/, the policy
    # is core/deny.toml, and the graph is walked from the core.
    module.WINDOWS = CORE
    module.ROOT_PACKAGE = ROOT_PACKAGE
    return module


def build(notices, targets):
    policy = notices.LicensePolicy()
    compiled = {}
    for target in targets:
        packages, _ = notices.dependency_packages(target)
        for package in packages:
            compiled[package["id"]] = package
    local, crates = notices.split_packages(list(compiled.values()))
    expressions, supplements = notices.check_packages(policy, local, crates)
    own = notices.own_license(local)

    out = notices.Notices()
    out.add(
        "AudioSync for macOS: notices of the Rust crates in the audio core",
        "",
        "Targets: {}".format(", ".join(targets)),
        "Generated from core/Cargo.lock by scripts/core_notices.py.",
        "",
        "The app binary contains AudioSync's audio core (core/), a Rust library,",
        "with AudioSync's own AirPlay sender (airplay/), Google Cast sender",
        "(cast/) and web remote (remote/), all AudioSync's own code ({}),".format(own),
        "compiled together with the",
        "crates below. Each is listed",
        "with its license; the full license and notice texts follow. The Rust",
        "standard library's notices are in the files next to this one.",
        "",
        "Contents",
        "  1. AudioSync",
        "  2. Rust crates ({})".format(len(crates)),
    )

    out.heading("1. AudioSync")
    core = notices.find_package(local, ROOT_PACKAGE)
    out.add("", "AudioSync's own code, {}: {}.".format(
        own, ", ".join("{} {}".format(p["name"], p["version"]) for p in sorted(local, key=lambda p: p["name"]))),
        core.get("repository") or "")
    out.text("LICENSE", notices.read_text(os.path.join(REPO, "LICENSE")), "AudioSync")

    out.heading("2. Rust crates ({})".format(len(crates)))
    notices.write_crates(out, policy, crates, expressions, supplements)
    out.add("")
    return out.lines, len(crates)


def main():
    parser = argparse.ArgumentParser(description="Write the notices of the Rust crates in the macOS app's core.")
    parser.add_argument("targets", nargs="+", metavar="target",
                        help="Rust target triple, e.g. aarch64-apple-darwin")
    parser.add_argument("--output", required=True, help="file to write")
    args = parser.parse_args()
    for target in args.targets:
        if not re.match(r"^[a-z0-9_]+-apple-darwin$", target):
            parser.error("'{}' isn't a macOS target triple".format(target))
    notices = shared()
    try:
        lines, count = build(notices, args.targets)
    except notices.NoticeError as error:
        print("core_notices.py: {}".format(error), file=sys.stderr)
        return 1
    os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
    with open(args.output, "w", encoding="utf-8", newline="\n") as handle:
        handle.write("\n".join(lines))
    print("Wrote {}: {} crates".format(args.output, count))
    return 0


if __name__ == "__main__":
    sys.exit(main())
