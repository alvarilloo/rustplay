"""Builds the flat RustPlay logo.

Writes crates/gui/assets/logo.svg from the traced outline in logo-shapes.txt (play triangle and
waves, potrace output in a 1024×572 canvas) on an orange rounded square, then renders
logo-master.png with headless Microsoft Edge. Afterwards run make_assets.py.

Usage: python -I packaging/tools/make_logo.py
"""
import re
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "crates/gui/assets"
SHAPES = Path(__file__).with_name("logo-shapes.txt")
EDGE = Path(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe")

ORANGE = "#FF5A00"
WHITE = "#FFFFFF"
# Rounded square of the original 1024×572 artwork, and the margin kept around it in the icon.
SQUARE = (316, 90, 392)
CORNER = 86
MARGIN = 0.04


def shapes_path():
    """The traced path without its first sub-path (the full-canvas background), made absolute."""
    data = " ".join(SHAPES.read_text(encoding="utf-8").split())
    start = re.match(r"M\s*(-?\d+)\s+(-?\d+)", data)
    sx, sy = int(start.group(1)), int(start.group(2))
    rest = data[data.index("z") + 1:].strip()
    move = re.match(r"m\s*(-?\d+)\s+(-?\d+)", rest)
    x, y = sx + int(move.group(1)), sy + int(move.group(2))
    return f"M{x} {y}" + rest[move.end():]


def svg():
    x, y, side = SQUARE
    pad = side * MARGIN / (1 - 2 * MARGIN)
    box = f"{x - pad:.1f} {y - pad:.1f} {side + 2 * pad:.1f} {side + 2 * pad:.1f}"
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{box}">\n'
        f'  <rect x="{x}" y="{y}" width="{side}" height="{side}" rx="{CORNER}" fill="{ORANGE}"/>\n'
        f'  <path transform="translate(0 572) scale(0.1 -0.1)" fill="{WHITE}" d="{shapes_path()}"/>\n'
        "</svg>\n"
    )


def render_png(svg_file: Path, out: Path, size: int = 1024):
    html = svg_file.with_suffix(".html")
    html.write_text(
        '<html><body style="margin:0;background:transparent">'
        f'<img src="{svg_file.as_uri()}" width="{size}" height="{size}"></body></html>',
        encoding="utf-8",
    )
    subprocess.run(
        [str(EDGE), "--headless=new", "--disable-gpu", "--allow-file-access-from-files",
         "--default-background-color=00000000", "--hide-scrollbars", f"--window-size={size},{size}",
         f"--screenshot={out}", html.as_uri()],
        check=True, capture_output=True,
    )


def main():
    text = svg()
    (ASSETS / "logo.svg").write_text(text, encoding="utf-8")
    with tempfile.TemporaryDirectory() as tmp:
        copy = Path(tmp) / "logo.svg"
        copy.write_text(text, encoding="utf-8")
        render_png(copy, ASSETS / "logo-master.png")
    print("logo.svg y logo-master.png generados")


if __name__ == "__main__":
    main()
