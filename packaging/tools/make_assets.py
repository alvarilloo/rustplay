"""Generates the app icon and the MSIX visual assets from the master logo.

Usage: python -I packaging/tools/make_assets.py
Reads crates/gui/assets/logo-master.png (transparent, square).
"""
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
MASTER = ROOT / "crates/gui/assets/logo-master.png"
ASSETS = ROOT / "packaging/msix/Assets"


def fit(logo, size, fraction):
    """Places the logo centred on a transparent canvas, using `fraction` of the shorter side."""
    w, h = size
    side = max(1, round(min(w, h) * fraction))
    canvas = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    canvas.alpha_composite(logo.resize((side, side), Image.LANCZOS), ((w - side) // 2, (h - side) // 2))
    return canvas


def main():
    logo = Image.open(MASTER).convert("RGBA")
    ASSETS.mkdir(parents=True, exist_ok=True)
    outputs = {}
    for scale in (100, 200):
        k = scale / 100
        px = lambda v: round(v * k)
        outputs[f"Square44x44Logo.scale-{scale}.png"] = fit(logo, (px(44), px(44)), 1.0)
        outputs[f"Square150x150Logo.scale-{scale}.png"] = fit(logo, (px(150), px(150)), 0.66)
        outputs[f"Wide310x150Logo.scale-{scale}.png"] = fit(logo, (px(310), px(150)), 0.66)
        outputs[f"SmallTile.scale-{scale}.png"] = fit(logo, (px(71), px(71)), 0.7)
        outputs[f"LargeTile.scale-{scale}.png"] = fit(logo, (px(310), px(310)), 0.6)
        outputs[f"StoreLogo.scale-{scale}.png"] = fit(logo, (px(50), px(50)), 1.0)
        outputs[f"SplashScreen.scale-{scale}.png"] = fit(logo, (px(620), px(300)), 0.6)
    for size in (16, 24, 32, 48, 256):
        icon = fit(logo, (size, size), 1.0)
        outputs[f"Square44x44Logo.targetsize-{size}.png"] = icon
        outputs[f"Square44x44Logo.targetsize-{size}_altform-unplated.png"] = icon
        outputs[f"Square44x44Logo.targetsize-{size}_altform-lightunplated.png"] = icon
    for name, image in outputs.items():
        image.save(ASSETS / name)

    gui = ROOT / "crates/gui/assets"
    logo.resize((256, 256), Image.LANCZOS).save(gui / "logo.png")
    logo.save(gui / "rustplay.ico", sizes=[(s, s) for s in (16, 20, 24, 32, 40, 48, 64, 128, 256)])
    print(f"{len(outputs)} recursos en {ASSETS}")


if __name__ == "__main__":
    main()
