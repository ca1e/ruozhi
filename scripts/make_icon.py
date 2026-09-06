#!/usr/bin/env python3
"""Generate assets/ruozhi.ico from assets/icon_1024.png (Windows exe icon).

Cross-platform counterpart of make_icon.sh (which needs sips/iconutil and
only runs on macOS). Requires Pillow:

    python -m pip install pillow
    python scripts/make_icon.py

The PNG is AI-generated (agnes-image) + corner-masked; regenerate it however
you like, then rerun this script.
"""

import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "icon_1024.png"
DST = ROOT / "assets" / "ruozhi.ico"

# Standard Windows icon sizes; BMP entries render everywhere, no Vista+
# PNG-icon support needed (file size is ~350 KB, irrelevant inside an exe).
SIZES = [16, 24, 32, 48, 64, 128, 256]


def main() -> int:
    if not SRC.exists():
        print(f"{SRC} missing", file=sys.stderr)
        return 1
    img = Image.open(SRC).convert("RGBA")
    if img.width != img.height:
        print(f"{SRC} is not square ({img.width}x{img.height})", file=sys.stderr)
        return 1
    img.save(DST, format="ICO", sizes=[(s, s) for s in SIZES], bitmap_format="bmp")

    with Image.open(DST) as ico:
        print(f"built {DST}")
        print(f"  frames: {sorted(ico.info.get('sizes', []))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
