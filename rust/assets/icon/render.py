"""Render the AstralGuard icon: SVG sources -> PNGs and a multi-size astralguard.ico.

Needs resvg on PATH (`cargo install resvg`). Python standard library only.
Run from anywhere:  python rust/assets/icon/render.py

Outputs (committed, so a normal build needs neither Python nor resvg):
  astralguard.ico        16-256 px; build.rs embeds it in astralguard.exe, the
                         installer uses it too
  astralguard-256.png    the README header and the Linux hicolor icon
  astralguard-64.png     the dashboard's window/taskbar icon
"""

import pathlib
import struct
import subprocess
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
LARGE = HERE / "astralguard.svg"
SMALL = HERE / "astralguard-small.svg"  # 32 px and below: no connector detail
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def render(svg: pathlib.Path, size: int, out: pathlib.Path) -> bytes:
    subprocess.run(["resvg", "-w", str(size), "-h", str(size), str(svg), str(out)], check=True)
    return out.read_bytes()


def write_ico(images: list[tuple[int, bytes]], out: pathlib.Path) -> None:
    """ICO with PNG-compressed entries (Windows Vista and later)."""
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, blobs = b"", b""
    for size, png in images:
        dim = 0 if size >= 256 else size  # 0 means 256 in an ICONDIRENTRY
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset)
        blobs += png
        offset += len(png)
    out.write_bytes(header + entries + blobs)


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        images = []
        for size in ICO_SIZES:
            svg = SMALL if size <= 32 else LARGE
            images.append((size, render(svg, size, tmp / f"{size}.png")))
        write_ico(images, HERE / "astralguard.ico")
    render(LARGE, 256, HERE / "astralguard-256.png")
    render(LARGE, 64, HERE / "astralguard-64.png")
    print("wrote astralguard.ico, astralguard-256.png, astralguard-64.png")


if __name__ == "__main__":
    main()
