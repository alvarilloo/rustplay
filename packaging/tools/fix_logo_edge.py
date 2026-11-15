"""Replaces the whitish rim around the master logo (left over from the source image's light
background) with a light orange edge taken from the colours just inside it.

Usage: python -I packaging/tools/fix_logo_edge.py   (then run make_assets.py)
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageFilter

ROOT = Path(__file__).resolve().parents[2]
MASTER = ROOT / "crates/gui/assets/logo-master.png"
RIM = 9          # px of the outer band to repaint
LIGHTEN = 0.28   # how much lighter than the neighbouring orange


def main():
    logo = Image.open(MASTER).convert("RGBA")
    r, g, b, alpha = logo.split()
    solid = alpha.point(lambda v: 255 if v > 250 else 0)
    inner = solid.filter(ImageFilter.MinFilter(2 * RIM + 1))
    ring = ImageChops.subtract(alpha, inner)

    # Spread the interior colours outwards: blur colour*mask and divide by the blurred mask.
    rgb = Image.merge("RGB", (r, g, b))
    black = Image.new("RGB", logo.size, (0, 0, 0))
    inside = Image.composite(rgb, black, inner)
    spread_rgb = inside.filter(ImageFilter.GaussianBlur(RIM * 1.6))
    spread_a = inner.filter(ImageFilter.GaussianBlur(RIM * 1.6))
    out = spread_rgb.load()
    weights = spread_a.load()
    width, height = logo.size
    for y in range(height):
        for x in range(width):
            wgt = weights[x, y]
            if wgt:
                cr, cg, cb = out[x, y]
                k = 255 / wgt
                cr, cg, cb = min(255, cr * k), min(255, cg * k), min(255, cb * k)
                out[x, y] = (
                    int(cr + (255 - cr) * LIGHTEN),
                    int(cg + (230 - cg) * LIGHTEN),
                    int(cb + (200 - cb) * LIGHTEN),
                )
    edge = Image.merge("RGBA", (*spread_rgb.split(), ring))
    base = Image.merge("RGBA", (r, g, b, ImageChops.subtract(alpha, ring)))
    fixed = Image.alpha_composite(base, edge)
    fixed.putalpha(alpha)
    fixed.save(MASTER)
    print(f"borde corregido en {MASTER}")


if __name__ == "__main__":
    main()
