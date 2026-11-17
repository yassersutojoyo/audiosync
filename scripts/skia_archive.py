"""Packs the Skia that skia-bindings built from source in a cargo build into
the archive its binary cache downloads: STORE/<tag>/skia-binaries-<key>.tar.gz.

Usage: skia_archive.py <profile dir, e.g. target/x86_64-pc-windows-msvc/release> <store dir>
       [<tag> <key>]

The tag and key come from the build script's output, where a failed download
names them; an offline build (SKIA_SOURCE_DIR) prints neither, so pass them.
"""

import io
import pathlib
import re
import sys
import tarfile

# What skia-bindings' binary cache imports: the libraries it links, the
# bindings and, with text layout, ICU's data; ninja leaves more in the folder.
NAMES = re.compile(
    r"^((skia|skia-bindings|skparagraph|skshaper|skunicode_core|skunicode_icu|svg|skresources|skottie|sksg|jsonreader)"
    r"\.lib|bindings\.rs|icudtl\.dat)$"
)
TRIED = re.compile(r"TRYING TO DOWNLOAD AND INSTALL SKIA BINARIES: (\S+)/(\S+)")


def main(argv: list[str]) -> int:
    if len(argv) not in (3, 5):
        print(__doc__, file=sys.stderr)
        return 2
    profile, store = pathlib.Path(argv[1]), pathlib.Path(argv[2])
    builds = sorted(
        (p for p in profile.glob("build/skia-bindings-*/out/skia") if (p / "bindings.rs").is_file()),
        key=lambda p: (p / "bindings.rs").stat().st_mtime,
    )
    if not builds:
        print(f"no Skia built under {profile}", file=sys.stderr)
        return 1
    skia = builds[-1]
    if len(argv) == 5:
        tag, key = argv[3], argv[4]
    else:
        found = TRIED.search((skia.parent.parent / "output").read_text(encoding="utf-8", errors="replace"))
        if not found:
            print(f"no tag and key in {skia.parent.parent / 'output'}", file=sys.stderr)
            return 1
        tag, key = found.groups()
    files = sorted(p for p in skia.iterdir() if p.is_file() and NAMES.match(p.name))
    archive = store / tag / f"skia-binaries-{key}.tar.gz"
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "w:gz") as tar:
        for path in files:
            tar.add(path, arcname=f"skia-binaries/{path.name}")
        for name, text in (("tag.txt", tag), ("key.txt", key)):
            data = text.encode()
            info = tarfile.TarInfo(f"skia-binaries/{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    print(f"{archive}: {', '.join(p.name for p in files)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
