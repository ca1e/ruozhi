#!/bin/sh
# Rebuild assets/ruozhi.icns from assets/icon_1024.png (1024x1024 RGBA).
# The PNG is AI-generated (agnes-image) + corner-masked; regenerate it
# however you like, then rerun this script.
set -e
cd "$(dirname "$0")/.."
[ -f assets/icon_1024.png ] || { echo "assets/icon_1024.png missing"; exit 1; }

rm -rf assets/icon.iconset
mkdir -p assets/icon.iconset
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon_1024.png --out assets/icon.iconset/icon_${s}x${s}.png >/dev/null
  d=$((s * 2))
  sips -z $d $d assets/icon_1024.png --out assets/icon.iconset/icon_${s}x${s}@2x.png >/dev/null
done
iconutil -c icns assets/icon.iconset -o assets/ruozhi.icns
echo "built assets/ruozhi.icns"
