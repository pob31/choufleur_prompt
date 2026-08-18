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
#   To notarize, one of these two sets — missing both, it builds without and says so,
#   and the DMG then needs right-click-Open on anybody else's machine:
#     APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_KEY_PATH
#                            an App Store Connect key: its id, the issuer uuid, and the
#                            path to the AuthKey_<id>.p8 file itself.
#     APPLE_ID, APPLE_PASSWORD (app-specific), APPLE_TEAM_ID
#
# Put them in scripts/.env.release, which is not committed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app="$here/../server/crates/choufleur-app"
# shellcheck disable=SC1091
[ -f "$here/.env.release" ] && source "$here/.env.release"

cd "$app"

# Strict, when this build is going to be handed to somebody.
#
# The bundler only *warns* when it cannot sign or notarize: it prints a line and
# produces a DMG anyway. That is right for a local build and completely wrong for a
# release, where the warning scrolls past in a log nobody reads and the artifact is
# published looking finished. Strict turns every one of those into a failure.
strict="${CHOUFLEUR_RELEASE_STRICT:-0}"

# Two ways to prove who you are to the notary service, and the bundler takes either:
# an Apple ID with an app-specific password, or an App Store Connect API key. The key
# is the better one — it belongs to the team rather than to one person's account, and
# it does not stop working when that account's password changes — so it is what CI
# uses. Locally, either.
notary=""
if [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_ISSUER:-}" ] && [ -n "${APPLE_API_KEY_PATH:-}" ]; then
  notary="apikey"
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
  notary="appleid"
fi

missing=""
[ -z "$notary" ] && missing="$missing notarization-credentials"
if [ "$strict" = "1" ]; then
  [ -z "${APPLE_CERTIFICATE:-}" ] && missing="$missing APPLE_CERTIFICATE"
  [ -z "${APPLE_CERTIFICATE_PASSWORD:-}" ] && missing="$missing APPLE_CERTIFICATE_PASSWORD"
fi

if [ "$strict" = "1" ] && [ -n "$missing" ]; then
  echo "these are not set, and a release cannot be signed or notarized without them:"
  for m in $missing; do echo "  $m"; done
  echo
  echo "For notarization, either APPLE_API_KEY + APPLE_API_ISSUER + APPLE_API_KEY_PATH"
  echo "(an App Store Connect key) or APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID."
  echo
  echo "Add them as repository secrets. Without them this would still produce a DMG —"
  echo "one that every other Mac refuses to open, which is the failure worth catching"
  echo "here rather than at a get-in."
  exit 1
fi

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
  echo "! no APPLE_SIGNING_IDENTITY — building ad-hoc signed, for this machine only"
else
  echo "signing as: $APPLE_SIGNING_IDENTITY"
fi
if [ -n "$missing" ]; then
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
# Not allowed to end the script: under `set -e` a bad signature would exit here, and
# the entitlement and Gatekeeper sections below — which say *what* is wrong — would
# never print. The verdict is carried to the end instead.
signed=0
codesign --verify --deep --strict --verbose=2 "$appdir" 2>&1 | sed 's/^/  /' || signed=1

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
assess=0
spctl --assess --type execute --verbose=2 "$appdir" 2>&1 | sed 's/^/  /' || assess=1
staple=0
xcrun stapler validate "$appdir" 2>&1 | sed 's/^/  /' || staple=1
# The disk image needs its own ticket, and it is the file people actually download.
#
# The bundler notarizes and staples the .app, then builds the image from it and only
# *signs* that — tauri-bundler 2.9.4's bundle/macos/dmg/mod.rs contains no notarize
# call at all. Gatekeeper assesses the image in its own right, so an unnotarized DMG
# reports `rejected / source=Unnotarized Developer ID` however good the app inside it
# is. Submitting it here is the difference between a colleague double-clicking and a
# colleague being told the file is damaged.
dmg="$(ls "$bundle/dmg/"*.dmg 2>/dev/null | head -1 || true)"
if [ -n "$dmg" ] && [ -n "$notary" ]; then
  if [ "$notary" = "apikey" ]; then
    set -- --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER"
  else
    set -- --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID"
  fi
  echo
  echo "── notarizing the disk image ──"
  # `--timeout` so an Apple-side hang is an error rather than a job that runs until
  # the runner gives up.
  if ! sub=$(xcrun notarytool submit "$dmg" --wait --timeout 30m --output-format json "$@"); then
    printf '%s\n' "$sub"
    # A rejection says only `status: Invalid`. The reason is in the log, and without
    # fetching it here the failure is unactionable.
    id=$(printf '%s' "$sub" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("id",""))' 2>/dev/null || true)
    if [ -n "$id" ]; then
      xcrun notarytool log "$id" "$@" || true
    fi
    echo "Apple did not accept the disk image — the log above says why."
    exit 1
  fi
  xcrun stapler staple "$dmg" || staple=1
fi
if [ -n "$dmg" ]; then
  xcrun stapler validate "$dmg" 2>&1 | sed 's/^/  /' || staple=1
fi

if [ "$strict" = "1" ] && { [ "$signed" != "0" ] || [ "$assess" != "0" ] || [ "$staple" != "0" ]; }; then
  echo
  echo "This build is not something to hand anybody: Gatekeeper rejects it, or the"
  echo "notarization ticket is not stapled. Published as-is, the first thing a"
  echo "colleague would see is macOS refusing to open it."
  exit 1
fi
