#!/usr/bin/env python3
"""Checks a release appcast.xml against docs/ENGINE_SPEC.md S311.

The appcast must hold exactly one Mac item, plus one item each for
windows-x64 and windows-arm64 when Windows zips are given, and no Windows
item when none are. Every item has one enclosure with a sparkle:edSignature,
a url ending in releases/download/v<version>/<zip name> and a length; every
item carries the release's sparkle:shortVersionString and one shared
sparkle:version. Each zip given must match its item's url name and length,
and each Windows zip must hold exactly audiosync.exe and
THIRD-PARTY-NOTICES.txt at its top level. A zip whose name has windows-x64
or windows-arm64 in it is that architecture's; any other zip is the Mac's.

It does not verify signatures: it only checks that sparkle:edSignature is a
well-formed Ed25519 signature. Sparkle's updater on the Mac and AudioSync's
own on Windows check the signature against the public key.

Exit status 0 means the appcast passes; 1 prints one line per problem.

scripts/sign_appcast.sh also uses it to write the Windows items: --add takes
a Windows zip and the signature Sparkle's sign_update made over it, adds its
item next to the Mac item in APPCAST, then checks the result. --strays fails
on Windows-looking zips in a folder that aren't named as the script expects.

Usage:
    scripts/check_appcast.py APPCAST VERSION [ZIP...]
    scripts/check_appcast.py APPCAST VERSION ZIP... --add ZIP SIGNATURE [--add ZIP SIGNATURE]
    scripts/check_appcast.py --strays FOLDER VERSION
    scripts/check_appcast.py --self-test
"""

import argparse
import base64
import contextlib
import io
import os
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile
from xml.sax.saxutils import escape, quoteattr

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
WINDOWS_ARCHES = ("windows-x64", "windows-arm64")
MAC_OS = ("", "macos")
WINDOWS_ZIP_FILES = {"audiosync.exe", "THIRD-PARTY-NOTICES.txt"}
ED25519_SIGNATURE_BYTES = 64


def sparkle(name):
    return f"{{{SPARKLE}}}{name}"


def zip_os(path):
    name = os.path.basename(path)
    for arch in WINDOWS_ARCHES:
        if arch in name:
            return arch
    return "macos"


def item_value(item, enclosure, name):
    child = item.find(sparkle(name))
    if child is not None and child.text and child.text.strip():
        return child.text.strip()
    if enclosure is not None:
        return enclosure.get(sparkle(name))
    return None


def is_signature(text):
    try:
        return len(base64.b64decode(text or "", validate=True)) == ED25519_SIGNATURE_BYTES
    except ValueError:
        return False


def windows_zip_name(version, arch):
    return f"AudioSync-{version}-{arch}.zip"


def stray_zips(folder, version):
    """Zips in folder that look like Windows zips but that sign_appcast.sh won't pick up."""
    wanted = {windows_zip_name(version, arch) for arch in WINDOWS_ARCHES}
    return sorted(name for name in os.listdir(folder)
                  if name.endswith(".zip") and "windows" in name.lower() and name not in wanted)


def windows_zip_problems(path):
    try:
        with zipfile.ZipFile(path) as archive:
            names = set(archive.namelist())
    except (OSError, zipfile.BadZipFile) as error:
        return [f"{path}: not a readable zip ({error})"]
    if names == WINDOWS_ZIP_FILES:
        return []
    want = ", ".join(sorted(WINDOWS_ZIP_FILES))
    got = ", ".join(sorted(names)) or "nothing"
    return [f"{path}: holds {got}; expected exactly {want} at the top level"]


def check(appcast, version, zips):
    """One line per way the appcast and zips break S311; empty when they pass."""
    version = version[1:] if version.startswith("v") else version
    try:
        root = ET.parse(appcast).getroot()
    except (OSError, ET.ParseError) as error:
        return [f"{appcast}: not a readable appcast ({error})"]
    problems = []
    zip_by_os = {}
    for path in zips:
        os_name = zip_os(path)
        if os_name in zip_by_os:
            problems.append(f"{path}: a second {os_name} zip (first: {zip_by_os[os_name]})")
            continue
        zip_by_os[os_name] = path
        if os_name != "macos":
            problems += windows_zip_problems(path)
    given_windows = [arch for arch in WINDOWS_ARCHES if arch in zip_by_os]
    if given_windows:
        problems += [f"no {arch} zip given next to the other Windows zip"
                     for arch in WINDOWS_ARCHES if arch not in zip_by_os]

    items_by_os = {}
    build_numbers = set()
    url_prefix = f"releases/download/v{version}/"
    for number, item in enumerate(root.iter("item"), 1):
        label = f"item {number}"
        enclosures = item.findall("enclosure")
        if len(enclosures) != 1:
            problems.append(f"{label}: {len(enclosures)} enclosures; expected 1")
        enclosure = enclosures[0] if enclosures else None
        os_name = enclosure.get(sparkle("os"), "") if enclosure is not None else ""
        if os_name in MAC_OS:
            os_name = "macos"
        elif os_name not in WINDOWS_ARCHES:
            problems.append(f"{label}: sparkle:os {os_name!r} is neither the Mac's nor "
                            f"{' nor '.join(WINDOWS_ARCHES)}")
        label = f"item {number} ({os_name})"
        items_by_os.setdefault(os_name, []).append(item)

        short = item_value(item, enclosure, "shortVersionString")
        if short != version:
            problems.append(f"{label}: sparkle:shortVersionString {short!r}; expected {version!r}")
        build = item_value(item, enclosure, "version")
        if not build:
            problems.append(f"{label}: no sparkle:version")
        else:
            build_numbers.add(build)
        if enclosure is None:
            continue
        signature = enclosure.get(sparkle("edSignature"))
        if not signature:
            problems.append(f"{label}: no sparkle:edSignature")
        elif not is_signature(signature):
            problems.append(f"{label}: sparkle:edSignature is not a base64 Ed25519 signature")
        url = enclosure.get("url", "")
        head, _, tail = url.rpartition("/")
        if not (head + "/").endswith(url_prefix) or not tail or any(c in tail for c in "?#"):
            problems.append(f"{label}: url {url!r} does not end in {url_prefix}<zip name>")
        length = enclosure.get("length", "")
        if not length.isdigit() or int(length) == 0:
            problems.append(f"{label}: length {length!r} is not a byte count")
        path = zip_by_os.get(os_name)
        if path is None:
            continue
        name = os.path.basename(path)
        if not url.endswith(url_prefix + name):
            problems.append(f"{label}: url {url!r} does not end in {url_prefix}{name}")
        size = os.path.getsize(path)
        if length != str(size):
            problems.append(f"{label}: length {length!r}; {name} is {size} bytes")

    if len(build_numbers) > 1:
        problems.append(f"items carry different sparkle:version values: {', '.join(sorted(build_numbers))}")
    macs = len(items_by_os.get("macos", []))
    if macs != 1:
        problems.append(f"{macs} Mac items; expected 1")
    for arch in WINDOWS_ARCHES:
        count = len(items_by_os.get(arch, []))
        want = 1 if given_windows else 0
        if count != want:
            reason = "with Windows zips given" if given_windows else "without Windows zips"
            problems.append(f"{count} {arch} items; expected {want} {reason}")
    return problems


def add_windows(text, version, signed):
    """The appcast text with one item per (Windows zip, signature) added after the Mac item.

    Each item takes the Mac item's versions and publication date, and its URL
    sits in the Mac item's download folder.
    """
    # Edits the feed after generate_appcast: fine only while SURequireSignedFeed is
    # off, since turning it on would make these edits break the feed's signature.
    version = version[1:] if version.startswith("v") else version
    root = ET.fromstring(text)
    macs = [item for item in root.iter("item")
            if len(item.findall("enclosure")) == 1
            and item.find("enclosure").get(sparkle("os"), "") in MAC_OS]
    if len(macs) != 1:
        raise ValueError(f"{len(macs)} Mac items; expected 1")
    mac = macs[0]
    enclosure = mac.find("enclosure")
    build = item_value(mac, enclosure, "version")
    short = item_value(mac, enclosure, "shortVersionString")
    if not build or short != version:
        raise ValueError(f"the Mac item's versions are {build!r} and {short!r}, not those of {version}")
    folder = enclosure.get("url", "").rpartition("/")[0]
    published = mac.findtext("pubDate")
    blocks = []
    for path, signature in signed:
        name = os.path.basename(path)
        arch = next((arch for arch in WINDOWS_ARCHES if name == windows_zip_name(version, arch)), None)
        if arch is None:
            raise ValueError(f"{name} is not named AudioSync-{version}-windows-x64.zip or -arm64.zip")
        if not is_signature(signature):
            raise ValueError(f"the signature given for {name} is not a base64 Ed25519 signature")
        lines = ["        <item>", f"            <title>{escape(short)}</title>"]
        if published:
            lines.append(f"            <pubDate>{escape(published)}</pubDate>")
        lines += [
            f"            <sparkle:version>{escape(build)}</sparkle:version>",
            f"            <sparkle:shortVersionString>{escape(short)}</sparkle:shortVersionString>",
            f"            <enclosure url={quoteattr(folder + '/' + name)} length=\"{os.path.getsize(path)}\""
            f" type=\"application/octet-stream\" sparkle:os=\"{arch}\""
            f" sparkle:edSignature={quoteattr(signature)}/>",
            "        </item>",
        ]
        blocks.append("\n".join(lines) + "\n")
    end = text.rindex("</channel>")
    line_start = text.rfind("\n", 0, end) + 1
    if text[line_start:end].strip():
        line_start = end
    return text[:line_start] + "".join(blocks) + text[line_start:]


def write_zip(path, files):
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in files.items():
            archive.writestr(name, data)


def write_appcast(path, items):
    body = "".join(
        "<item><title>{v}</title>"
        "<sparkle:version>{build}</sparkle:version>"
        "<sparkle:shortVersionString>{v}</sparkle:shortVersionString>"
        '<enclosure url="{url}" length="{length}" type="application/octet-stream"{os}{sig}/>'
        "</item>".format(**item)
        for item in items
    )
    with open(path, "w", encoding="utf-8") as f:
        f.write('<?xml version="1.0" encoding="utf-8"?>\n'
                f'<rss version="2.0" xmlns:sparkle="{SPARKLE}"><channel>'
                f"<title>AudioSync</title>{body}</channel></rss>\n")


FIXTURE_SIGNATURE = base64.b64encode(bytes(range(ED25519_SIGNATURE_BYTES))).decode()


def fixture_item(zip_path, version="0.8.0", build="80", os_attr=None, sig=True,
                 length=None, url_name=None):
    name = url_name or os.path.basename(zip_path)
    return {
        "v": version,
        "build": build,
        "url": f"https://github.com/o/r/releases/download/v0.8.0/{name}",
        "length": os.path.getsize(zip_path) if length is None else length,
        "os": f' sparkle:os="{os_attr}"' if os_attr else "",
        "sig": f' sparkle:edSignature="{FIXTURE_SIGNATURE}"' if sig else "",
    }


def self_test():
    # covers: S311
    failures = []
    cases = 0
    with tempfile.TemporaryDirectory() as folder:
        mac = os.path.join(folder, "AudioSync-0.8.0.zip")
        x64 = os.path.join(folder, "AudioSync-0.8.0-windows-x64.zip")
        arm = os.path.join(folder, "AudioSync-0.8.0-windows-arm64.zip")
        extra = os.path.join(folder, "AudioSync-0.8.0-windows-arm64-extra.zip")
        nested = os.path.join(folder, "nested", "AudioSync-0.8.0-windows-arm64.zip")
        os.mkdir(os.path.dirname(nested))
        write_zip(mac, {"AudioSync.app/Contents/Info.plist": "plist"})
        windows = {"audiosync.exe": "MZ", "THIRD-PARTY-NOTICES.txt": "notices"}
        write_zip(x64, windows)
        write_zip(arm, {**windows, "audiosync.exe": "MZ arm"})
        write_zip(extra, {**windows, "readme.txt": "x"})
        write_zip(nested, {"audiosync/audiosync.exe": "MZ", "THIRD-PARTY-NOTICES.txt": "n"})

        def good():
            return [fixture_item(mac),
                    fixture_item(x64, os_attr="windows-x64"),
                    fixture_item(arm, os_attr="windows-arm64")]

        def run(name, items, zips, want, version="0.8.0"):
            nonlocal cases
            cases += 1
            appcast = os.path.join(folder, f"{name}.xml")
            write_appcast(appcast, items)
            got = check(appcast, version, zips)
            if want is None:
                if got:
                    failures.append(f"{name}: expected a pass, got {got}")
            elif not any(want in line for line in got):
                failures.append(f"{name}: expected a line with {want!r}, got {got}")

        everything = [mac, x64, arm]
        run("full release", good(), everything, None)
        run("tag form of the version", good(), everything, None, version="v0.8.0")
        run("Mac only, no zips", good()[:1], [], None)
        run("Mac only, Mac zip", good()[:1], [mac], None)
        run("macos named on the Mac item",
            [fixture_item(mac, os_attr="macos")] + good()[1:], everything, None)
        run("Windows items without Windows zips", good(), [mac], "expected 0 without")
        run("an arch item missing", good()[:2], everything, "0 windows-arm64 items")
        run("only one Windows zip", good()[:2], [mac, x64], "no windows-arm64 zip")
        run("two Mac items", good() + good()[:1], everything, "2 Mac items")
        run("no Mac item", good()[1:], everything, "0 Mac items")
        bad_os = good()
        bad_os[2]["os"] = ' sparkle:os="windows-arm"'
        run("unknown sparkle:os", bad_os, everything, "'windows-arm' is neither")
        unsigned = good()
        unsigned[1]["sig"] = ""
        run("no edSignature", unsigned, everything, "no sparkle:edSignature")
        run("short version differs",
            good()[:2] + [fixture_item(arm, version="0.7.9", os_attr="windows-arm64")],
            everything, "'0.7.9'; expected '0.8.0'")
        run("build number differs",
            good()[:2] + [fixture_item(arm, build="81", os_attr="windows-arm64")],
            everything, "different sparkle:version")
        run("wrong length",
            good()[:1] + [fixture_item(x64, length=7, os_attr="windows-x64")] + good()[2:],
            everything, "length '7'")
        run("url names another zip",
            good()[:1] + [fixture_item(x64, os_attr="windows-x64",
                                       url_name="AudioSync-0.8.0-windows-arm64.zip")] + good()[2:],
            everything, f"does not end in releases/download/v0.8.0/{os.path.basename(x64)}")
        wrong_tag = good()
        wrong_tag[0]["url"] = wrong_tag[0]["url"].replace("v0.8.0", "v0.7.1")
        run("url under another tag", wrong_tag, everything, "does not end in")
        run("zip with an extra file", good(), [mac, x64, extra], "readme.txt")
        run("zip with the exe in a folder", good()[:2] + [fixture_item(nested,
            os_attr="windows-arm64")], [mac, x64, nested], "audiosync/audiosync.exe")
        two_enclosures = os.path.join(folder, "two enclosures.xml")
        write_appcast(two_enclosures, good())
        with open(two_enclosures, encoding="utf-8") as f:
            text = f.read().replace("</item>", '<enclosure url="x" length="1"/></item>', 1)
        with open(two_enclosures, "w", encoding="utf-8") as f:
            f.write(text)
        cases += 1
        if not any("2 enclosures" in line for line in check(two_enclosures, "0.8.0", everything)):
            failures.append("two enclosures: not reported")
        broken = os.path.join(folder, "broken.xml")
        with open(broken, "w", encoding="utf-8") as f:
            f.write("<rss><channel><item>")
        cases += 1
        if not any("not a readable appcast" in line for line in check(broken, "0.8.0", [])):
            failures.append("broken XML: not reported")

        short_signature = good()
        short_signature[1]["sig"] = ' sparkle:edSignature="c2ln"'
        run("edSignature too short for Ed25519", short_signature, everything, "not a base64 Ed25519")

        def added(name, signed, zips, want):
            nonlocal cases
            cases += 1
            appcast = os.path.join(folder, f"{name}.xml")
            write_appcast(appcast, good()[:1])
            with open(appcast, encoding="utf-8") as f:
                text = f.read()
            try:
                text = add_windows(text, "0.8.0", signed)
            except ValueError as error:
                if want != "refused":
                    failures.append(f"{name}: add_windows refused it ({error})")
                return
            if want == "refused":
                failures.append(f"{name}: add_windows took it")
                return
            with open(appcast, "w", encoding="utf-8") as f:
                f.write(text)
            got = check(appcast, "0.8.0", zips)
            if want is None and got:
                failures.append(f"{name}: expected a pass, got {got}")
            elif want is not None and not any(want in line for line in got):
                failures.append(f"{name}: expected a line with {want!r}, got {got}")

        added("items add_windows writes", [(x64, FIXTURE_SIGNATURE), (arm, FIXTURE_SIGNATURE)],
              everything, None)
        added("add_windows with the x64 zip only", [(x64, FIXTURE_SIGNATURE)], everything,
              "0 windows-arm64 items")
        added("add_windows given a value that is not a signature", [(x64, "c2ln")], everything, "refused")
        added("add_windows given a zip under another name", [(extra, FIXTURE_SIGNATURE)], everything, "refused")
        cases += 1
        if stray_zips(folder, "0.8.0") != [os.path.basename(extra)]:
            failures.append(f"stray Windows zips: expected only {os.path.basename(extra)}, "
                            f"got {stray_zips(folder, '0.8.0')}")
        for name, where, want in (("a folder with a stray", folder, 1),
                                  ("a folder without one", os.path.dirname(nested), 0)):
            cases += 1
            with contextlib.redirect_stderr(io.StringIO()):
                got = main(["--strays", where, "0.8.0"])
            if got != want:
                failures.append(f"--strays on {name}: exit {got}, expected {want}")
    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    print(f"self-test: {cases} cases, {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv):
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0],
        epilog="It does not verify signatures: it only checks that sparkle:edSignature "
               "is a well-formed Ed25519 signature. Exit 0 means the appcast passes; "
               "otherwise one line per problem.",
    )
    parser.add_argument("--self-test", action="store_true",
                        help="check the checker on fixture appcasts and zips in a temp folder")
    parser.add_argument("--add", nargs=2, action="append", default=[], metavar=("ZIP", "SIGNATURE"),
                        help="add a Windows zip's item, signed by sign_update, before the check")
    parser.add_argument("--strays", nargs=2, metavar=("FOLDER", "VERSION"),
                        help="fail on Windows-looking zips in FOLDER that aren't named for VERSION")
    parser.add_argument("appcast", nargs="?", help="the release's appcast.xml")
    parser.add_argument("version", nargs="?", help="the release version, such as 0.8.0 or v0.8.0")
    parser.add_argument("zips", nargs="*", help="the release's zips (Mac and Windows)")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.strays:
        folder, version = args.strays
        version = version[1:] if version.startswith("v") else version
        strays = stray_zips(folder, version)
        for name in strays:
            print(f"{name} is not named AudioSync-{version}-windows-x64.zip or -arm64.zip, so it "
                  f"would get no appcast item: rename it or move it away", file=sys.stderr)
        return 1 if strays else 0
    if not args.appcast or not args.version:
        parser.error("APPCAST and VERSION are required")
    if args.add:
        try:
            with open(args.appcast, encoding="utf-8") as f:
                text = add_windows(f.read(), args.version, [tuple(pair) for pair in args.add])
        except (OSError, ET.ParseError, ValueError) as error:
            print(f"{args.appcast}: {error}")
            return 1
        with open(args.appcast, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
    problems = check(args.appcast, args.version, args.zips)
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
