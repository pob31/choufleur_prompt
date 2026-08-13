#!/usr/bin/env bash
# Fetch the speech models Choufleur needs into server/models/.
#
# Models are not committed: the Whisper weights alone are hundreds of megabytes,
# and they carry their own licence (MIT, from OpenAI's release) rather than
# Choufleur's. Nothing here talks to the network at run time — this script is the
# only step that does, and only once.
#
#   ./scripts/fetch-models.sh              # small (multilingual) + Silero VAD
#   ./scripts/fetch-models.sh --medium     # also fetch medium, for non-English shows
#   ./scripts/fetch-models.sh --list       # print what is installed and its hash
#
# If your venue network blocks these hosts, download the files by hand from the
# URLs printed below and drop them into server/models/ under the same names.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODELS="$ROOT/server/models"

# Multilingual weights only. The `.en` variants are English-only and would be
# silently useless the first time a French line arrives.
WHISPER_BASE="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
# Pinned release rather than master: the v4 → v5 change altered the model's input
# tensors, and a silently swapped file would be a subtle accuracy bug rather than
# a loud failure.
SILERO_TAG="v5.1.2"
SILERO_URLS=(
  "https://raw.githubusercontent.com/snakers4/silero-vad/${SILERO_TAG}/src/silero_vad/data/silero_vad.onnx"
  "https://raw.githubusercontent.com/snakers4/silero-vad/${SILERO_TAG}/files/silero_vad.onnx"
)

want_medium=0
list_only=0
for arg in "$@"; do
  case "$arg" in
    --medium) want_medium=1 ;;
    --list) list_only=1 ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

hash_of() { shasum -a 256 "$1" | cut -d' ' -f1; }

if [ "$list_only" = 1 ]; then
  if [ ! -d "$MODELS" ]; then echo "no models installed ($MODELS does not exist)"; exit 0; fi
  found=0
  for f in "$MODELS"/*; do
    [ -e "$f" ] || continue
    found=1
    printf '%-28s %10s  %s\n' "$(basename "$f")" "$(du -h "$f" | cut -f1)" "$(hash_of "$f")"
  done
  [ "$found" = 1 ] || echo "no models installed"
  exit 0
fi

mkdir -p "$MODELS"

# fetch <destination-name> <url> [<url fallback>...]
fetch() {
  local name="$1"; shift
  local dest="$MODELS/$name"
  if [ -s "$dest" ]; then
    echo "have    $name  ($(hash_of "$dest"))"
    return 0
  fi
  local tmp="$dest.part"
  for url in "$@"; do
    echo "fetch   $name"
    echo "        <- $url"
    if curl -fL --progress-bar -o "$tmp" "$url"; then
      mv "$tmp" "$dest"
      echo "        sha256 $(hash_of "$dest")"
      return 0
    fi
    rm -f "$tmp"
    echo "        failed, trying next source" >&2
  done
  echo "could not fetch $name — download it by hand into $MODELS/" >&2
  return 1
}

fetch "ggml-small.bin" "$WHISPER_BASE/ggml-small.bin"
[ "$want_medium" = 1 ] && fetch "ggml-medium.bin" "$WHISPER_BASE/ggml-medium.bin"
fetch "silero_vad.onnx" "${SILERO_URLS[@]}"

cat <<EOF

Models are in $MODELS (gitignored).

Record the hashes above in your project notes on first fetch, then compare with
'--list' after any re-download: a model swapped underneath you changes every
tracking number in the eval, and does so quietly.

Licences: Whisper weights are MIT (OpenAI); Silero VAD is MIT (snakers4).
Neither is part of Choufleur's source.
EOF
