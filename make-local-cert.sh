#!/bin/bash
#
# Creates the local code-signing identity "AudioSync Local Code Signing" in a
# dedicated keychain, fully unattended (no password prompt, no trust settings).
#
# Why it's needed: without it, build.sh signs ad-hoc. An ad-hoc signature's
# cdhash changes on every build, and TCC identifies the app by it, so the
# "System Audio Recording" permission is invalidated on every build — the toggle
# in Settings stays on but capture is silent. Signed with a fixed certificate,
# the designated requirement becomes `identifier … and certificate leaf = H"…"`,
# which survives rebuilds, so the permission is granted once and kept.
#
# The certificate does not need to be trusted: codesign accepts an untrusted
# identity when it is named by its SHA-1 hash, and TCC only matches the
# designated requirement. Keeping it out of the login keychain means no
# "codesign wants to access key" prompt and no admin password.
#
# What it does (does nothing if the identity already exists):
#   1) creates ~/Library/Keychains/audiosync-signing.keychain-db with a random
#      password stored in ~/Library/Application Support/AudioSync/ (mode 600)
#   2) creates an RSA key + self-signed certificate (EKU codeSigning, 10 years)
#   3) imports it, allowing codesign to use the key without a prompt
#
# Security note: on a contributor's Mac the key only signs local builds. On the
# Mac that builds releases it also signs the public AudioSync releases, so it is
# the release key: anyone who can read that home folder could sign code that
# macOS treats as AudioSync for privacy permissions. Keep a private backup of
# the keychain and its signing-keychain.pass file (in the AudioSync Application
# Support folder). If either is lost, this script creates a new identity and
# users must grant the privacy permissions again.
#
# To undo (on the release Mac, only after backing both up):
#   security delete-keychain ~/Library/Keychains/audiosync-signing.keychain-db
#   rm -rf ~/Library/Application\ Support/AudioSync/signing-keychain.pass
#
set -euo pipefail

NAME="AudioSync Local Code Signing"
KEYCHAIN="$HOME/Library/Keychains/audiosync-signing.keychain-db"
PASS_FILE="$HOME/Library/Application Support/AudioSync/signing-keychain.pass"
DAYS=3650
OPENSSL=/usr/bin/openssl
[ -x "$OPENSSL" ] || OPENSSL=$(command -v openssl)

identity_hash() {
  security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null \
    | awk -v n="\"$NAME\"" 'index($0, n) { print $2; exit }'
}

if [ -f "$KEYCHAIN" ] && [ -f "$PASS_FILE" ]; then
  # Never replace an existing identity on a transient failure: a new key changes
  # the app's designated requirement and resets its privacy permissions.
  if ! security unlock-keychain -p "$(cat "$PASS_FILE")" "$KEYCHAIN"; then
    echo "❌ Could not unlock $KEYCHAIN. To start over, delete it and" >&2
    echo "   $PASS_FILE, then run this script again (permissions must then be re-granted)." >&2
    exit 1
  fi
  if [ -n "$(identity_hash)" ]; then
    echo "✅ Already present: \"$NAME\" ($(identity_hash)) — skipping"
    exit 0
  fi
  echo "▶ The keychain exists but holds no identity — recreating it"
elif [ -f "$KEYCHAIN" ] || [ -f "$PASS_FILE" ]; then
  echo "▶ Only half of the signing setup exists (keychain or password file) — recreating both"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/localcert-XXXXXX")
trap 'rm -rf "$TMP"' EXIT

# Start clean: a keychain without its password file (or without the identity)
# cannot be used unattended.
[ -f "$KEYCHAIN" ] && security delete-keychain "$KEYCHAIN"
mkdir -p "$(dirname "$PASS_FILE")"
( umask 077; "$OPENSSL" rand -hex 24 > "$PASS_FILE" )
PASS=$(cat "$PASS_FILE")

echo "▶ Creating keychain: $KEYCHAIN"
security create-keychain -p "$PASS" "$KEYCHAIN"
security set-keychain-settings "$KEYCHAIN"          # no auto-lock timeout
security unlock-keychain -p "$PASS" "$KEYCHAIN"

cat > "$TMP/cert.cnf" <<EOF
[req]
distinguished_name = dn
prompt = no
x509_extensions = ext
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF

echo "▶ Creating self-signed certificate: \"$NAME\" (${DAYS} days)"
"$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days "$DAYS" -sha256 \
  -config "$TMP/cert.cnf" -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null

# macOS `security` cannot read PKCS#12 files made with OpenSSL 3+ defaults; the
# system LibreSSL uses compatible 3DES, OpenSSL 3+ needs -legacy.
P12_OPTS=()
"$OPENSSL" version | grep -q LibreSSL || P12_OPTS=(-legacy)
P12_PASS=$("$OPENSSL" rand -hex 16)
"$OPENSSL" pkcs12 -export ${P12_OPTS[@]+"${P12_OPTS[@]}"} -name "$NAME" \
  -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/cert.p12" -passout "pass:$P12_PASS"

security import "$TMP/cert.p12" -k "$KEYCHAIN" -f pkcs12 -P "$P12_PASS" -T /usr/bin/codesign >/dev/null
# Let codesign use the key without a GUI prompt.
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASS" "$KEYCHAIN" >/dev/null

HASH=$(identity_hash)
if [ -z "$HASH" ]; then
  echo "❌ Identity not found after import. Check: security find-identity -p codesigning \"$KEYCHAIN\"" >&2
  exit 1
fi
echo "✅ Done: \"$NAME\" ($HASH) — ./build.sh signs with it from now on"
