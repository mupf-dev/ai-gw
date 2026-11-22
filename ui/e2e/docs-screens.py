# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
"""Screenshots für docs/konzept/ux/ übernehmen: nur Desktop hell/dunkel, die das UX-Konzept einbindet,
verkleinert (1200 px) und auf eine Palette reduziert (Ziel: zusammen ca. 1 MB).

Alle übrigen Varianten (schmal, weitere Zustände) bleiben Artefakte von ui/e2e/run.sh (results/screens/).
Aufruf (Pillow nötig):  python3 ui/e2e/docs-screens.py ui/e2e/results/screens docs/konzept/ux
"""
import pathlib
import re
import sys

from PIL import Image

src, dst = map(pathlib.Path, sys.argv[1:3])
doc = (dst.parent / "04-ux-designsystem.md").read_text()
wanted = set(re.findall(r"ux/([a-z0-9-]+-(?:hell|dunkel)-desktop\.png)", doc))
dst.mkdir(parents=True, exist_ok=True)
for old in dst.glob("*.png"):
    old.unlink()
total = 0
for f in sorted(src.glob("*-desktop.png")):
    if f.name not in wanted:
        continue
    img = Image.open(f).convert("RGB")
    # Breite 1440 → 1200 px genügt für die Doku; Palette mit 64 Farben (Text bleibt scharf genug).
    if img.width > 1200:
        img = img.resize((1200, round(img.height * 1200 / img.width)), Image.LANCZOS)
    out = dst / f.name
    img.quantize(colors=64, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(out, optimize=True)
    total += out.stat().st_size
print(f"{len(list(dst.glob('*.png')))} Dateien, {total / 1024:.0f} KiB")
