#!/usr/bin/env bash
# Makes a test-signed copy of a package from make_msix.sh, for a sideload check
# on a PC that trusts the test certificate; the Store signs what it gets itself.
# A new throwaway key every run:
#
#   scripts/sign_msix_test.sh <AudioSync-X.Y.Z-x64.msix> <out-dir>
#
# Writes into <out-dir>, which must be outside the repository: the signed copy
# (<name>-test-signed.msix), the certificate to trust (AudioSync-test.cer) and
# its key with no password (AudioSync-test.pfx). Imports nothing into any
# certificate store. Runs under Git Bash; SignTool comes from $SIGNTOOL or the
# newest Windows 10/11 SDK installed.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <AudioSync-X.Y.Z-x64.msix> <out-dir>" >&2
  exit 2
fi
msix=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
# Windows paths ignore case, and a worktree's main checkout counts as the repository too.
main=$(cd "$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)/.." && pwd)
for top in "$repo" "$main"; do
  case "${out,,}/" in
    "${top,,}"/*) echo "<out-dir> is inside the repository; the key must not land there" >&2; exit 2 ;;
  esac
done

# Windows installs the package only when the certificate's subject is the
# manifest's Publisher.
publisher=$(sed -n 's/^ *Publisher="\(CN=[^",]*\)".*$/\1/p' "$repo/desktop/msix/AppxManifest.xml")
[ -n "$publisher" ] || { echo "no single-name Publisher (CN=...) in desktop/msix/AppxManifest.xml" >&2; exit 1; }

signtool=${SIGNTOOL:-$(find "/c/Program Files (x86)/Windows Kits/10/bin" -ipath '*/x64/signtool.exe' 2>/dev/null | sort -V | tail -n 1)}
[ -n "$signtool" ] || { echo "signtool.exe not found; install a Windows 10/11 SDK" >&2; exit 1; }

key="$out/AudioSync-test.key"
cert="$out/AudioSync-test.pem"
pfx="$out/AudioSync-test.pfx"
signed="$out/$(basename "$msix" .msix)-test-signed.msix"
trap 'rm -f "$key" "$cert"' EXIT
# MSYS would rewrite the subject and SignTool's switches as paths.
MSYS2_ARG_CONV_EXCL='*' openssl req -quiet -x509 -newkey rsa:3072 -sha256 -days 30 -nodes \
  -subj "/$publisher" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=codeSigning" \
  -keyout "$(cygpath -w "$key")" -out "$(cygpath -w "$cert")"
MSYS2_ARG_CONV_EXCL='*' openssl pkcs12 -export -passout pass: \
  -inkey "$(cygpath -w "$key")" -in "$(cygpath -w "$cert")" -out "$(cygpath -w "$pfx")"
MSYS2_ARG_CONV_EXCL='*' openssl x509 -outform der \
  -in "$(cygpath -w "$cert")" -out "$(cygpath -w "$out/AudioSync-test.cer")"
rm -f "$key" "$cert"

cp "$msix" "$signed"
MSYS2_ARG_CONV_EXCL='*' "$signtool" sign /fd SHA256 /f "$(cygpath -w "$pfx")" "$(cygpath -w "$signed")"
echo "Test-signed package: $signed"
echo "Its certificate: $out/AudioSync-test.cer"
