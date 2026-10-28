#!/usr/bin/env python3
"""Checks a release appcast.xml against docs/ENGINE_SPEC.md S311.

The appcast must hold exactly one Mac item and one item each for
windows-x64 and windows-arm64; with --mac-only it must hold the Mac item and
no Windows item, and refuses Windows zips. A release is taken to carry
Windows unless --mac-only says otherwise, so a forgotten Windows zip fails
here instead of leaving Windows users without the update. Every item has
one enclosure with a sparkle:edSignature, a url ending in
releases/download/v<version>/<zip name> and a length; every
item carries the release's sparkle:shortVersionString and one shared
sparkle:version. Each zip given must match its item's url name and length,
and each Windows zip must hold exactly audiosync.exe and
THIRD-PARTY-NOTICES.txt at its top level, with an audiosync.exe whose
version information names VERSION as its ProductVersion and marks it as a
release build (AudioSyncReleaseBuild 1). A zip whose name has windows-x64
or windows-arm64 in it is that architecture's; any other zip is the Mac's.

It does not verify signatures: it only checks that sparkle:edSignature is a
well-formed Ed25519 signature. Sparkle's updater on the Mac and AudioSync's
own on Windows check the signature against the public key.

Exit status 0 means the appcast passes; 1 prints one line per problem.

scripts/sign_appcast.sh also uses it to write the Windows items: --add takes
a Windows zip and the signature Sparkle's sign_update made over it, adds its
item next to the Mac item in APPCAST, then checks the result. --strays fails
on Windows-looking zips in a folder that aren't named as the script expects.

--assets checks a release's asset names, before upload and after: every zip
among them needs an appcast item whose sparkle:os matches the zip, and every
item's zip must be among them. The names come as arguments (paths count by
file name) or, with --assets-file, from a file holding the output of
`gh release view TAG --json assets` or one name per line. It also wants a
Mac item and, without --mac-only, an item for each Windows architecture.
With the gh output, each item's length must also be its zip's attached size.

Usage:
    scripts/check_appcast.py [--mac-only] APPCAST VERSION [ZIP...]
    scripts/check_appcast.py APPCAST VERSION ZIP... --add ZIP SIGNATURE [--add ZIP SIGNATURE]
    scripts/check_appcast.py --strays FOLDER VERSION
    scripts/check_appcast.py --assets [--mac-only] APPCAST VERSION NAME...
    scripts/check_appcast.py --assets [--mac-only] APPCAST VERSION --assets-file FILE
    scripts/check_appcast.py --self-test
"""

import argparse
import base64
import contextlib
import io
import json
import os
import struct
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile
from xml.sax.saxutils import escape, quoteattr

SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
WINDOWS_ARCHES = ("windows-x64", "windows-arm64")
MAC_OS = ("", "macos")
MAC_ONLY_HINT = "pass --mac-only for a release without Windows"
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


def version_strings(exe):
    """The text values of the VS_VERSIONINFO resource in an exe's bytes, by name
    (Microsoft Learn, VS_VERSIONINFO, StringFileInfo and String)."""
    start = exe.find("VS_VERSION_INFO\0".encode("utf-16-le")) - 6
    if start < 0:
        return {}
    blob = exe[start:start + struct.unpack_from("<H", exe, start)[0]]
    strings = {}

    def walk(at, end):
        while at + 6 <= end:
            length, value_length, text = struct.unpack_from("<HHH", blob, at)
            if length < 6:
                return
            key_end = at + 6
            while key_end + 2 <= len(blob) and blob[key_end:key_end + 2] != b"\0\0":
                key_end += 2
            value = (key_end + 5) & ~3
            # A text value's length counts UTF-16 units, any other value's bytes.
            size = value_length * 2 if text else value_length
            if text and value_length:
                strings[blob[at + 6:key_end].decode("utf-16-le")] = \
                    blob[value:value + size].decode("utf-16-le").rstrip("\0")
            walk((value + size + 3) & ~3, min(at + length, end))
            at = (at + length + 3) & ~3

    walk(0, len(blob))
    return strings


def windows_zip_problems(path, version):
    try:
        with zipfile.ZipFile(path) as archive:
            names = set(archive.namelist())
            exe = archive.read("audiosync.exe") if names == WINDOWS_ZIP_FILES else None
    except (OSError, zipfile.BadZipFile) as error:
        return [f"{path}: not a readable zip ({error})"]
    if exe is None:
        want = ", ".join(sorted(WINDOWS_ZIP_FILES))
        got = ", ".join(sorted(names)) or "nothing"
        return [f"{path}: holds {got}; expected exactly {want} at the top level"]
    try:
        strings = version_strings(exe)
    except (struct.error, UnicodeDecodeError):
        strings = {}
    problems = []
    # An older exe in a newer zip would be offered this update again after every install.
    if strings.get("ProductVersion") != version:
        problems.append(f"{path}: audiosync.exe has ProductVersion "
                        f"{strings.get('ProductVersion')!r}; expected {version!r}")
    if strings.get("AudioSyncReleaseBuild") != "1":
        problems.append(f"{path}: audiosync.exe is not a release build, so it never checks for "
                        "updates: take it from the CI run of the push to release/x.y.z (RELEASE.md)")
    return problems


def check(appcast, version, zips, mac_only=False):
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
            if mac_only:
                problems.append(f"{path}: a Windows zip, but --mac-only says the release has none")
            else:
                problems += windows_zip_problems(path, version)
    if zips and not mac_only:
        problems += [f"no {arch} zip given ({MAC_ONLY_HINT})"
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
        want = 0 if mac_only else 1
        if count != want:
            reason = "with --mac-only" if mac_only else f"({MAC_ONLY_HINT})"
            problems.append(f"{count} {arch} items; expected {want} {reason}")
    return problems


def read_assets(path):
    """Asset names and sizes from `gh release view TAG --json assets` output, or names one per
    line (size None); - is stdin."""
    if path == "-":
        text = sys.stdin.read()
    else:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    if text.lstrip().startswith(("{", "[")):
        listed = json.loads(text)
        if isinstance(listed, dict):
            listed = listed.get("assets", [])
        return {entry["name"] if isinstance(entry, dict) else str(entry):
                entry.get("size") if isinstance(entry, dict) else None for entry in listed}
    return {line.strip(): None for line in text.splitlines() if line.strip()}


def asset_problems(appcast, version, assets, mac_only=False):
    """One line per release zip without an appcast item, per item whose zip is not attached
    or whose length is not the attached size (when assets maps a name to one), and per OS the
    release must reach that has no item."""
    version = version[1:] if version.startswith("v") else version
    try:
        root = ET.parse(appcast).getroot()
    except (OSError, ET.ParseError) as error:
        return [f"{appcast}: not a readable appcast ({error})"]
    url_prefix = f"releases/download/v{version}/"
    item_zips = set()
    item_oses = set()
    problems = []
    for number, item in enumerate(root.iter("item"), 1):
        for enclosure in item.findall("enclosure"):
            url = enclosure.get("url", "")
            name = url.rpartition("/")[2]
            item_zips.add(name)
            if name not in assets:
                problems.append(f"item {number}: url {url!r} names {name}, which is not attached "
                                f"to the v{version} release")
            elif not url.endswith(url_prefix + name):
                problems.append(f"item {number}: url {url!r} does not end in {url_prefix}{name}")
            elif assets[name] is not None and enclosure.get("length") != str(assets[name]):
                problems.append(f"item {number}: length {enclosure.get('length')!r}; the attached "
                                f"{name} is {assets[name]} bytes")
            # The Windows updater picks an enclosure by sparkle:os alone, never by its url.
            os_name = enclosure.get(sparkle("os"), "")
            os_name = "macos" if os_name in MAC_OS else os_name
            item_oses.add(os_name)
            if mac_only and os_name in WINDOWS_ARCHES:
                problems.append(f"item {number}: a {os_name} item, but --mac-only says the "
                                f"release has none")
            if os_name != zip_os(name):
                problems.append(f"item {number}: {name} with sparkle:os {os_name!r}; "
                                f"expected {zip_os(name)!r}")
    for name in sorted(assets):
        if name.endswith(".zip") and name not in item_zips:
            problems.append(f"{name} is attached but has no appcast item, so "
                            f"{'Windows' if zip_os(name) != 'macos' else 'Mac'} users never see this release")
    if "macos" not in item_oses:
        problems.append("no Mac item in the appcast, so Mac users never see this release")
    if not mac_only:
        problems += [f"no {arch} item in the appcast, so Windows users never see this release "
                     f"({MAC_ONLY_HINT})" for arch in WINDOWS_ARCHES if arch not in item_oses]
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


def version_node(key, value=None, children=()):
    """A VS_VERSIONINFO node as the resource compiler lays it out; a str value is text."""
    text = isinstance(value, str)
    data = (value + "\0").encode("utf-16-le") if text else value or b""
    key_bytes = (key + "\0").encode("utf-16-le")
    node = key_bytes + bytes(-(6 + len(key_bytes)) % 4) + data
    for child in children:
        node += bytes(-(6 + len(node)) % 4) + child
    return struct.pack("<HHH", 6 + len(node), len(data) // 2 if text else len(data), text) + node


def fake_exe(version="0.8.0", release=True):
    strings = [version_node("ProductName", "AudioSync"), version_node("ProductVersion", version)]
    if release:
        strings.append(version_node("AudioSyncReleaseBuild", "1"))
    table = version_node("StringFileInfo", children=[version_node("040904B0", children=strings)])
    return b"MZ\0\0" + version_node("VS_VERSION_INFO", bytes(52), [table])


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
        windows = {"audiosync.exe": fake_exe(), "THIRD-PARTY-NOTICES.txt": "notices"}
        write_zip(x64, windows)
        write_zip(arm, {**windows, "audiosync.exe": fake_exe() + b"arm"})
        exes = {}
        for case, exe in (("old", fake_exe("0.7.1")), ("unmarked", fake_exe(release=False)),
                          ("plain", b"MZ")):
            exes[case] = os.path.join(folder, case, "AudioSync-0.8.0-windows-x64.zip")
            os.mkdir(os.path.dirname(exes[case]))
            write_zip(exes[case], {**windows, "audiosync.exe": exe})
        write_zip(extra, {**windows, "readme.txt": "x"})
        write_zip(nested, {"audiosync/audiosync.exe": "MZ", "THIRD-PARTY-NOTICES.txt": "n"})

        def good():
            return [fixture_item(mac),
                    fixture_item(x64, os_attr="windows-x64"),
                    fixture_item(arm, os_attr="windows-arm64")]

        def run(name, items, zips, want, version="0.8.0", mac_only=False):
            nonlocal cases
            cases += 1
            appcast = os.path.join(folder, f"{name}.xml")
            write_appcast(appcast, items)
            got = check(appcast, version, zips, mac_only)
            if want is None:
                if got:
                    failures.append(f"{name}: expected a pass, got {got}")
            elif not any(want in line for line in got):
                failures.append(f"{name}: expected a line with {want!r}, got {got}")

        everything = [mac, x64, arm]
        run("full release", good(), everything, None)
        run("tag form of the version", good(), everything, None, version="v0.8.0")
        run("Mac only, no zips", good()[:1], [], None, mac_only=True)
        run("Mac only, Mac zip", good()[:1], [mac], None, mac_only=True)
        run("Mac-only appcast without --mac-only", good()[:1], [mac], "0 windows-x64 items; expected 1")
        run("Mac-only appcast, no zips, without --mac-only", good()[:1], [], "0 windows-arm64 items; expected 1")
        run("macos named on the Mac item",
            [fixture_item(mac, os_attr="macos")] + good()[1:], everything, None)
        run("Windows items with --mac-only", good(), [mac], "expected 0 with --mac-only", mac_only=True)
        run("a Windows zip with --mac-only", good()[:1], [mac, x64], "--mac-only says", mac_only=True)
        run("Windows items, Mac zip only", good(), [mac], "no windows-x64 zip given")
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
        for case, want in (("old", "ProductVersion '0.7.1'; expected '0.8.0'"),
                           ("unmarked", "not a release build"),
                           ("plain", "ProductVersion None")):
            run(f"{case} audiosync.exe", good()[:1] + [fixture_item(exes[case], os_attr="windows-x64")]
                + good()[2:], [mac, exes[case], arm], want)
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

        def against_assets(name, items, assets, want, use_file=None, mac_only=False):
            nonlocal cases
            cases += 1
            appcast = os.path.join(folder, f"{name}.xml")
            write_appcast(appcast, items)
            argv = ["--assets", appcast, "0.8.0"] + (["--mac-only"] if mac_only else [])
            if use_file is None:
                argv += assets
            else:
                listing = os.path.join(folder, f"{name}.assets")
                with open(listing, "w", encoding="utf-8") as f:
                    f.write(use_file(assets))
                argv += ["--assets-file", listing]
            out = io.StringIO()
            try:
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
                    got = main(argv)
            except SystemExit as stop:
                got = f"exit {stop.code}"
            lines = out.getvalue()
            if want is None and got != 0:
                failures.append(f"{name}: expected a pass, got {got}: {lines!r}")
            elif want is not None and (got != 1 or want not in lines):
                failures.append(f"{name}: expected exit 1 and a line with {want!r}, got {got}: {lines!r}")

        names = [os.path.basename(path) for path in everything]
        other_assets = ["AudioSync-0.8.0.dmg", "SHA256SUMS.txt", "appcast.xml"]
        x64_name, arm_name = names[1], names[2]
        against_assets("assets of a full release", good(), names + other_assets, None)
        against_assets("assets of a Mac-only release", good()[:1], names[:1] + other_assets, None,
                       mac_only=True)
        against_assets("Windows items with --mac-only", good(), names + other_assets,
                       "a windows-x64 item, but --mac-only", mac_only=True)
        against_assets("no Mac item", good()[1:], names[1:] + other_assets, "no Mac item")
        against_assets("Mac-only assets without --mac-only", good()[:1], names[:1] + other_assets,
                       "no windows-x64 item")
        against_assets("Windows zips attached, appcast Mac-only", good()[:1], names + other_assets,
                       f"{x64_name} is attached but has no appcast item")
        against_assets("arm64 zip attached without its item", good()[:2], names + other_assets,
                       f"{arm_name} is attached but has no appcast item")
        against_assets("an item whose zip is not attached", good(), names[:2] + other_assets,
                       f"{arm_name}, which is not attached")
        against_assets("Mac zip attached without its item", good()[1:], names + other_assets,
                       f"{names[0]} is attached but has no appcast item")
        no_os = good()
        no_os[1]["os"] = ""
        against_assets("Windows item without sparkle:os", no_os, names + other_assets,
                       f"{x64_name} with sparkle:os 'macos'; expected 'windows-x64'")
        wrong_os = good()
        wrong_os[2]["os"] = ' sparkle:os="windows-x64"'
        against_assets("Windows item with the other arch's sparkle:os", wrong_os, names + other_assets,
                       f"{arm_name} with sparkle:os 'windows-x64'; expected 'windows-arm64'")
        mac_as_windows = good()
        mac_as_windows[0]["os"] = ' sparkle:os="windows-x64"'
        against_assets("Mac item marked windows-x64", mac_as_windows, names + other_assets,
                       f"{names[0]} with sparkle:os 'windows-x64'; expected 'macos'")
        against_assets("assets from gh release view --json assets", good()[:1], names + other_assets,
                       f"{x64_name} is attached but has no appcast item",
                       use_file=lambda listed: json.dumps(
                           {"assets": [{"name": n, "size": 1} for n in listed]}))
        against_assets("assets one name per line", good(), names + other_assets, None,
                       use_file=lambda listed: "\n".join(listed) + "\n")
        sizes = {os.path.basename(path): os.path.getsize(path) for path in everything}
        against_assets("attached sizes match the lengths", good(), names + other_assets, None,
                       use_file=lambda listed: json.dumps(
                           {"assets": [{"name": n, "size": sizes.get(n, 1)} for n in listed]}))
        against_assets("an attached zip whose size is not its length", good(), names + other_assets,
                       f"the attached {arm_name} is {sizes[arm_name] + 1} bytes",
                       use_file=lambda listed: json.dumps(
                           {"assets": [{"name": n, "size": sizes.get(n, 1) + (n == arm_name)}
                                       for n in listed]}))
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
    parser.add_argument("--assets", action="store_true",
                        help="compare the release's asset names (instead of ZIPs) with the appcast items")
    parser.add_argument("--mac-only", action="store_true",
                        help="the release has no Windows zips: want the Mac item only")
    parser.add_argument("--assets-file", metavar="FILE",
                        help="with --assets: read the names from FILE (gh release view --json "
                             "assets output, or one per line; - is stdin)")
    parser.add_argument("appcast", nargs="?", help="the release's appcast.xml")
    parser.add_argument("version", nargs="?", help="the release version, such as 0.8.0 or v0.8.0")
    parser.add_argument("zips", nargs="*", help="the release's zips (Mac and Windows)")
    args = parser.parse_intermixed_args(argv)
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
    if args.assets_file and not args.assets:
        parser.error("--assets-file needs --assets")
    if args.assets:
        if args.add:
            parser.error("--assets checks a finished appcast; it takes no --add")
        names = {os.path.basename(name): None for name in args.zips}
        if args.assets_file:
            try:
                names.update(read_assets(args.assets_file))
            except (OSError, ValueError, KeyError, TypeError) as error:
                print(f"{args.assets_file}: not a list of asset names ({error})")
                return 1
        if not names:
            parser.error("--assets needs the asset names, as arguments or with --assets-file")
        problems = asset_problems(args.appcast, args.version, names, args.mac_only)
        for problem in problems:
            print(problem)
        return 1 if problems else 0
    if args.add:
        if args.mac_only:
            parser.error("--add writes Windows items; it takes no --mac-only")
        try:
            with open(args.appcast, encoding="utf-8") as f:
                text = add_windows(f.read(), args.version, [tuple(pair) for pair in args.add])
        except (OSError, ET.ParseError, ValueError) as error:
            print(f"{args.appcast}: {error}")
            return 1
        with open(args.appcast, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
    problems = check(args.appcast, args.version, args.zips, args.mac_only)
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
