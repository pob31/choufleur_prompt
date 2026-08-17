#!/usr/bin/env bash
# Build Choufleur.app and its DMG, signed, and say plainly whether they are.
#
# Signing is not cosmetic here. macOS gives a process that has never been granted the
# microphone a stream that runs and delivers nothing at all — no error, no prompt —
# and only a bundle carrying NSMicrophoneUsageDescription is ever asked about. The
# entitlement has to be on the sidecar as well as the app: capture happens in the
# show server, and under the hardened runtime a binary without it is refused the
# microphone even after the operator has said yes.
#
# So this checks afterwards rather than trusting the bundler, and the checks are the
# point of the script.
#
#   APPLE_SIGNING_IDENTITY   'Developer ID Application: … (TEAMID)'; without it the
#                            build is ad-hoc signed — fine on this machine, refused
#                            on anybody else's.
#   APPLE_ID, APPLE_PASSWORD (app-specific), APPLE_TEAM_ID
#                            all three, to notarize. Missing, it builds without and
#                            says so; the DMG then needs right-click-Open elsewhere.
#
# Put them in scripts/.env.release, which is not committed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app="$here/../server/crates/choufleur-app"
# shellcheck disable=SC1091
[ -f "$here/.env.release" ] && source "$here/.env.release"

cd "$app"

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "! no APPLE_SIGNING_IDENTITY — building ad-hoc signed, for this machine only"
else
  echo "signing as: $APPLE_SIGNING_IDENTITY"
fi
if [ -z "${APPLE_ID:-}" ] || [ -z "${APPLE_PASSWORD:-}" ] || [ -z "${APPLE_TEAM_ID:-}" ]; then
  echo "! not notarizing — another Mac will refuse this build until it is"
fi

cargo tauri build

bundle="$here/../server/target/release/bundle"
appdir="$bundle/macos/Choufleur.app"
sidecar="$appdir/Contents/MacOS/choufleur-replay"

echo
echo "── what came out ──"
[ -d "$appdir" ] || { echo "no app bundle at $appdir"; exit 1; }
du -sh "$appdir"
ls "$bundle/dmg/"*.dmg 2>/dev/null || echo "(no dmg)"

echo
echo "── signature ──"
codesign --verify --deep --strict --verbose=2 "$appdir" 2>&1 | sed 's/^/  /'

echo
echo "── the microphone entitlement, on the sidecar ──"
# The one that is silently missing when capture mysteriously delivers nothing.
if [ -f "$sidecar" ]; then
  if codesign -d --entitlements - --xml "$sidecar" 2>/dev/null \
      | plutil -convert xml1 -o - - 2>/dev/null \
      | grep -q 'com.apple.security.device.audio-input'; then
    echo "  ok — the show server may open the microphone"
  else
    echo "  MISSING on $sidecar"
    echo "  Capture will deliver silence no matter what the operator allows."
    exit 1
  fi
else
  echo "  no sidecar at $sidecar — the app cannot start a server"
  exit 1
fi

echo
echo "── usage description ──"
plutil -extract NSMicrophoneUsageDescription raw "$appdir/Contents/Info.plist" \
  | sed 's/^/  /' || { echo "  MISSING — no prompt will ever appear"; exit 1; }

echo
echo "── would another Mac open it? ──"
spctl --assess --type execute --verbose=2 "$appdir" 2>&1 | sed 's/^/  /' || true
xcrun stapler validate "$appdir" 2>&1 | sed 's/^/  /' || true
