"""위젯 아이콘을 그려 src-tauri/icons 아래 PNG와 Windows용 멀티사이즈 BMP ICO를 만듭니다.

모양은 사용량 위젯 공통이다 — 모서리만 둥글게 깎은 단색 사각 타일 위에 흰 가로 바 세 줄.
위젯마다 BRAND 색만 다르다. app-icon.png(512px)는 미리보기용으로 함께 다시 그린다.
"""
from __future__ import annotations

import struct
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "app-icon.png"
ICONS = ROOT / "src-tauri" / "icons"
OUT_ICO = ICONS / "icon.ico"
# Standard Windows shell sizes — BMP (not PNG) entries embed reliably via winres.
# 첫 항목(16)이 Tauri 기본 창 아이콘이자 트레이 아이콘이 된다.
SIZES = [16, 32, 48, 256]

BRAND = (201, 100, 66, 255)
WHITE = (255, 255, 255, 255)
# 타일 한 변 대비 모서리 반지름.
CORNER_RATIO = 0.18
# 모서리·바 끝 안티에일리어싱용 배율.
SUPERSAMPLE = 8


def render(size: int) -> Image.Image:
    """size 픽셀 아이콘 한 장. 바 위치를 그 크기의 정수 픽셀에 맞춰 작은 크기에서도 번지지 않게 한다."""
    bar_h = max(2, round(size * 0.085))
    gap = max(1, round(size * 0.075))
    left = round(size * 0.19)
    right = round(size * 0.81)
    top = (size - (3 * bar_h + 2 * gap)) // 2

    k = SUPERSAMPLE
    big = Image.new("RGBA", (size * k, size * k), (0, 0, 0, 0))
    draw = ImageDraw.Draw(big)
    draw.rounded_rectangle(
        [0, 0, size * k - 1, size * k - 1], radius=round(size * k * CORNER_RATIO), fill=BRAND
    )
    for index in range(3):
        y = top + index * (bar_h + gap)
        draw.rounded_rectangle(
            [left * k, y * k, right * k - 1, (y + bar_h) * k - 1],
            radius=bar_h * k // 2,
            fill=WHITE,
        )
    return big.resize((size, size), Image.Resampling.LANCZOS)


def to_bmp_dib(im: Image.Image) -> bytes:
    """Return BITMAPINFOHEADER + BGRA XOR bitmap + AND mask (ICO style)."""
    im = im.convert("RGBA")
    w, h = im.size
    pixels = im.split()
    r, g, b, a = [ch.tobytes() for ch in pixels]
    bgra = bytearray(w * h * 4)
    for i in range(w * h):
        bgra[i * 4 + 0] = b[i]
        bgra[i * 4 + 1] = g[i]
        bgra[i * 4 + 2] = r[i]
        bgra[i * 4 + 3] = a[i]

    stride = w * 4
    xor = bytearray(stride * h)
    for row in range(h):
        src_off = row * stride
        dst_off = (h - 1 - row) * stride
        xor[dst_off : dst_off + stride] = bgra[src_off : src_off + stride]

    # 둥근 모서리 밖(알파 0)은 AND 마스크에도 투명으로 표시한다.
    # 알파를 읽지 않는 경로에서도 모서리가 검게 채워지지 않는다.
    and_row = ((w + 31) // 32) * 4
    and_mask = bytearray(and_row * h)
    for row in range(h):
        dst_off = (h - 1 - row) * and_row
        for col in range(w):
            if a[row * w + col] == 0:
                and_mask[dst_off + col // 8] |= 0x80 >> (col % 8)

    header = struct.pack(
        "<IIIHHIIIIII",
        40,
        w,
        h * 2,
        1,
        32,
        0,
        len(xor) + len(and_mask),
        0,
        0,
        0,
        0,
    )
    return header + bytes(xor) + bytes(and_mask)


def write_ico(frames: list[Image.Image], dest: Path) -> None:
    dibs = [to_bmp_dib(im) for im in frames]
    count = len(frames)
    offset = 6 + 16 * count
    entries = bytearray()
    blob = bytearray()
    for im, dib in zip(frames, dibs):
        w, h = im.size
        entries += struct.pack(
            "<BBBBHHII",
            0 if w >= 256 else w,
            0 if h >= 256 else h,
            0,
            0,
            1,
            32,
            len(dib),
            offset + len(blob),
        )
        blob += dib
    dest.write_bytes(struct.pack("<HHH", 0, 1, count) + entries + blob)


def refresh_extra_icons() -> None:
    """`tauri icon`이 만들어 둔 나머지 PNG(Square*Logo 등)와 icns도 같은 모양으로 맞춘다."""
    managed = {"32x32.png", "128x128.png", "128x128@2x.png", "icon.png"}
    for png in sorted(ICONS.glob("*.png")):
        if png.name in managed:
            continue
        with Image.open(png) as old:
            size = old.width
        render(size).save(png)
        print(f"refreshed {png.name} ({size}px)")
    icns = ICONS / "icon.icns"
    if icns.is_file():
        render(1024).save(icns)
        print(f"refreshed {icns.name}")


def main() -> None:
    render(512).save(SRC)
    print(f"drew {SRC}")

    ICONS.mkdir(parents=True, exist_ok=True)
    frames: list[Image.Image] = []
    for s in SIZES:
        im = render(s)
        frames.append(im)
        if s == 32:
            im.save(ICONS / "32x32.png")
        if s == 256:
            # Tauri conf also references these PNGs.
            render(128).save(ICONS / "128x128.png")
            im.save(ICONS / "128x128@2x.png")
            im.save(ICONS / "icon.png")

    write_ico(frames, OUT_ICO)
    print(f"wrote {OUT_ICO} bytes={OUT_ICO.stat().st_size} sizes={SIZES}")
    refresh_extra_icons()


if __name__ == "__main__":
    main()
