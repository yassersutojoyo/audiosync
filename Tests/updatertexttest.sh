#!/bin/bash
# Checks the text half of spec S308 on the Windows app's sources: the updater's
# buttons, menu items and notices take their words from desktop/ui/strings.slint.
# Contrast, focus and the screen reader need the running app, so not here.
# Usage: Tests/updatertexttest.sh (run by Tests/run-tests.sh). UPDATER_TEXT_UI
# points the quoted-title scan at another copy of desktop/ui.
# covers: S308 (its words: buttons, menu items and notices come from strings.slint)
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
strings=desktop/ui/strings.slint
failures=0
check() {   # check <name> <command...>
  local name=$1
  shift
  if "$@"; then
    echo "  PASS  $name"
  else
    echo "  FAIL  $name"
    failures=$((failures + 1))
  fi
}
block() {   # block <file> <ERE of its first line>: up to the brace that closes it
  # Through the environment: awk -v would eat the regex's backslashes.
  START=$2 awk '
    !on && $0 ~ ENVIRON["START"] { on = 1; indent = match($0, /[^ ]/) }
    on { print }
    on && /^ *}$/ && match($0, /[^ ]/) == indent { exit }
  ' "$1"
}
same() { [ "$1" = "$2" ]; }
none() { [ -z "$1" ]; }

buttons=$(block "$strings" 'public pure function update-button\(')
words=$(grep -o '@tr("[^"]*")' <<< "$buttons" | sed 's/^@tr("//; s/")$//' | sort)
want=$(printf '%s\n' "Download and Install" "Not Now" "Cancel" "Try Again" \
  "Open Release Page" "Restart Now" | sort)
check "update-button names the six buttons of S308" same "$words" "$want"

variants=$(sed -n 's/^export enum UpdateButton { \(.*\) }$/\1/p' desktop/ui/types.slint \
  | tr -d ' ' | tr ',' '\n' | grep -v '^restart-now$' | sort)
named=$(grep -o 'UpdateButton\.[a-z-]*' <<< "$buttons" | sed 's/^UpdateButton\.//' | sort -u)
check "update-button has a branch for every button but the last, Restart Now" \
  same "$named" "$variants"

loops=$(grep -n 'for .* in .*update-buttons' desktop/ui/*.slint || true)
check "a notice lists its update buttons somewhere" [ -n "$loops" ]
while IFS=: read -r file line _; do
  [ -n "$file" ] || continue
  body=$(sed -n "${line},$((line + 4))p" "$file")
  check "$file:$line labels each button with Strings.update-button" \
    grep -q 'text: Strings\.update-button(button)' <<< "$body"
  check "$file:$line names each button to a screen reader with Strings.update-button" \
    grep -q 'accessible-label: Strings\.update-button(button)' <<< "$body"
done <<< "$loops"

for item in check-updates-automatically menu-check-for-updates; do
  check "the menu item $item reads its title from strings.slint" \
    grep -q "title: Strings\.$item;" desktop/ui/main.slint
  check "strings.slint words the menu item $item" \
    grep -q "out property <string> $item: @tr(\"" "$strings"
done
strays=$(grep -n '"Check for Updates' desktop/ui/*.slint | grep -v "^$strings:" || true)
check "no other .slint file words a menu item itself" none "$strays"
# Every title in the UI (menu items, windows, sections) names a Strings
# property or a model field, so a quoted title is a word strings.slint lacks.
ui=${UPDATER_TEXT_UI:-desktop/ui}
quoted=$(grep -n '^ *title:.*"' "$ui"/*.slint | grep -v '/strings\.slint:' || true)
check "no title outside strings.slint is a quoted literal" none "$quoted"

notice=$(block desktop/src/view.rs '^fn update_notice_text\(')
check "view.rs words the updater's notice in update_notice_text" [ -n "$notice" ]
check "update_notice_text holds no text of its own" none "$(grep '"' <<< "$notice" || true)"
while read -r name; do
  check "update_notice_text uses $name" grep -q "_${name//-/_}(" <<< "$notice"
done < <(grep -o 'notice-update-[a-z-]*' "$strings" | sort -u)

if [ "$failures" -gt 0 ]; then
  echo "FAIL: $failures updater text check(s)"
  exit 1
fi
echo "PASSED"
