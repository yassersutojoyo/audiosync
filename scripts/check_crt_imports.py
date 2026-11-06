"""Fail when a Windows executable imports the Visual C++ runtime DLLs.

A fresh Windows has no VC++ redistributable, so an exe that imports
VCRUNTIME*, MSVCP* or api-ms-win-crt-* stops at launch with
STATUS_DLL_NOT_FOUND. Reads the PE import directory with the standard
library only, so it runs on any CI image.

Usage: python -I scripts/check_crt_imports.py <exe>
"""

import struct
import sys

FORBIDDEN = ("vcruntime", "msvcp", "api-ms-win-crt-")


def imported_dlls(data):
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe : pe + 4] != b"PE\0\0":
        raise ValueError("not a PE file")
    sections, opt_size = struct.unpack_from("<H12xH", data, pe + 6)
    opt = pe + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    # The data directories start after the PE32 (0x10B) or PE32+ (0x20B) fields.
    directories = opt + (96 if magic == 0x10B else 112)
    import_rva = struct.unpack_from("<I", data, directories + 8)[0]
    table = opt + opt_size
    spans = [
        struct.unpack_from("<4I", data, table + 40 * i + 8) for i in range(sections)
    ]

    def offset(rva):
        for size, address, raw_size, raw in spans:
            if address <= rva < address + max(size, raw_size):
                return rva - address + raw
        raise ValueError(f"RVA {rva:#x} is in no section")

    names = []
    if import_rva == 0:
        return names
    entry = offset(import_rva)
    while True:
        descriptor = struct.unpack_from("<5I", data, entry)
        if not any(descriptor):
            return names
        start = offset(descriptor[3])
        names.append(data[start : data.index(b"\0", start)].decode("ascii"))
        entry += 20


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: check_crt_imports.py <exe>")
    with open(sys.argv[1], "rb") as f:
        dlls = imported_dlls(f.read())
    bad = [d for d in dlls if d.lower().startswith(FORBIDDEN)]
    if bad:
        print(f"FAIL: {sys.argv[1]} imports the C runtime DLLs {', '.join(bad)};")
        print("  it will not start on a Windows without the Visual C++ runtime")
        sys.exit(1)
    print(f"OK: {sys.argv[1]} imports no C runtime DLL ({len(dlls)} DLLs)")


if __name__ == "__main__":
    main()
