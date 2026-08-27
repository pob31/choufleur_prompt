#!/bin/sh
# Serial-DFU a build onto a XIAO that is in its bootloader (double-tap reset;
# a fresh board on stock Arduino firmware also answers a 1200-baud touch).
#   tools/flash.sh build_debug     (or build_buzzer)
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
BUILD=${1:-build_debug}
HEX="$HERE/../$BUILD/app_buzzer/zephyr/zephyr.hex"
PORT=$(ls /dev/cu.usbmodem* | head -1)
"$HERE/.venv/bin/adafruit-nrfutil" dfu genpkg --dev-type 0x0052 --application "$HEX" /tmp/choufleur-buzzer.zip >/dev/null
"$HERE/.venv/bin/adafruit-nrfutil" dfu serial --package /tmp/choufleur-buzzer.zip -p "$PORT" -b 115200 --singlebank
