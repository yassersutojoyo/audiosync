#!/bin/bash
#
# Checks that makeicon.sh and make-web-icons.sh still reproduce the committed icons
# from the committed icon-source.png. Both scripts write into a temporary folder;
# the generated files must be exactly the committed ones by name (AppIcon.icns and
# every PNG and ICO in site/), each with the same images, pixel sizes and formats,
# and byte-identical or within a small pixel tolerance (scripts/brand/compare-icons.swift).
# Needs macOS (sips, iconutil, Swift). CI runs it on every push.
#
# Usage: scripts/brand/check-icons.sh
#
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/../.."

work=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-check-icons-XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir "$work/expected" "$work/actual" "$work/site"

./makeicon.sh icon-source.png "$work/actual/AppIcon.icns" >/dev/null
./make-web-icons.sh icon-source.png "$work/site" >/dev/null

# The committed icons and the generated ones, flattened into one list each.
cp AppIcon.icns "$work/expected/"
while IFS= read -r file; do
  cp "$file" "$work/expected/site-${file#site/}"
done < <(printf 'site/%s\n' apple-touch-icon.png favicon-16x16.png favicon-32x32.png favicon.ico \
  favicon.png icon-192.png icon-512.png icon.png mstile-150x150.png)
for file in "$work/site"/*; do
  cp "$file" "$work/actual/site-${file##*/}"
done

expected_list=$(cd "$work/expected" && ls)
actual_list=$(cd "$work/actual" && ls)
if [ "$expected_list" != "$actual_list" ]; then
  echo "The generated icons differ from the committed ones by name:"
  diff <(echo "$expected_list") <(echo "$actual_list") || true
  exit 1
fi

# site/icon.png is a plain copy of the source, so the committed one must match it byte for byte.
cmp -s icon-source.png site/icon.png || { echo "site/icon.png is not a copy of icon-source.png"; exit 1; }

echo "Comparing $(echo "$expected_list" | wc -l | tr -d ' ') icon files:"
# shellcheck disable=SC2086 # the names have no spaces; one argument per file
swift scripts/brand/compare-icons.swift "$work/expected" "$work/actual" $expected_list
echo "The icon scripts reproduce the committed icons."
