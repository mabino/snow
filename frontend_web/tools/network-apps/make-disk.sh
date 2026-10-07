#!/bin/sh
# Build the "Network Apps" AppleTalk test disk (Bolo, EZChat, MacPing,
# TeleTalk) into frontend_web/www/media/NetworkApps.dsk.
#
# The programs are 1990s shareware/demos downloaded from the Info-Mac
# archive; they are not part of this repository. Needs Docker.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:-"$HERE/../../www/media"}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
docker build -q -t snow-network-apps "$HERE" >/dev/null
docker run --rm -v "$OUT:/out" snow-network-apps /out/NetworkApps.dsk /tmp/work
echo "Add it as a second hard disk, e.g.:"
echo "  http://127.0.0.1:8080/?rom=media/<rom>&disk=media/<system 6 disk>&disk=media/NetworkApps.dsk"
