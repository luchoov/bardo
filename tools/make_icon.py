"""Draws the bardo icon: a lowercase "b" whose bowl is a sound wave, on a dark rounded tile.

Usage: python tools/make_icon.py [out_dir]   (default: app/assets; needs Pillow)
"""
import math
import sys
from PIL import Image, ImageDraw, ImageFilter

S = 1024
ACCENT_TOP = (255, 150, 92)
ACCENT_BOTTOM = (240, 90, 50)


def gradient(size, top, bottom):
    img = Image.new('RGBA', (size, size))
    px = img.load()
    for y in range(size):
        t = y / (size - 1)
        c = tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,)
        for x in range(size):
            px[x, y] = c
    return img


def render(detail=True):
    tile_mask = Image.new('L', (S, S), 0)
    ImageDraw.Draw(tile_mask).rounded_rectangle((40, 40, S - 40, S - 40), radius=230, fill=255)
    tile = gradient(S, (40, 42, 48), (20, 21, 25))
    out = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    out.paste(tile, (0, 0), tile_mask)
    # Hairline highlight along the tile's top edge.
    edge = Image.new('L', (S, S), 0)
    ImageDraw.Draw(edge).rounded_rectangle((40, 40, S - 40, S - 40), radius=230, outline=40, width=6)
    out.paste(Image.new('RGBA', (S, S), (255, 255, 255, 255)), (0, 0), edge)

    mark = Image.new('L', (S, S), 0)
    d = ImageDraw.Draw(mark)
    stem_w = 104
    x0, top, bottom = 268, 210, 806
    d.rounded_rectangle((x0, top, x0 + stem_w, bottom), radius=stem_w // 2, fill=255)
    # The bowl: a half ring hugging the lower stem.
    cx, cy = x0 + stem_w // 2, 594
    r_out, thick = 212, 104
    d.pieslice((cx - r_out, cy - r_out, cx + r_out, cy + r_out), -90, 90, fill=255)
    r_in = r_out - thick
    d.pieslice((cx - r_in, cy - r_in, cx + r_in, cy + r_in), -90, 90, fill=0)
    d.rounded_rectangle((x0, top, x0 + stem_w, bottom), radius=stem_w // 2, fill=255)
    if detail:
        # The outer sound wave.
        r2, t2 = 352, 74
        wave = Image.new('L', (S, S), 0)
        w = ImageDraw.Draw(wave)
        w.pieslice((cx - r2, cy - r2, cx + r2, cy + r2), -52, 52, fill=255)
        w.pieslice((cx - r2 + t2, cy - r2 + t2, cx + r2 - t2, cy + r2 - t2), -60, 60, fill=0)
        # Round caps on the wave's ends.
        for ang in (-52, 52):
            rm = r2 - t2 / 2
            px_, py_ = cx + rm * math.cos(math.radians(ang)), cy + rm * math.sin(math.radians(ang))
            w.ellipse((px_ - t2 / 2, py_ - t2 / 2, px_ + t2 / 2, py_ + t2 / 2), fill=255)
        mark = Image.composite(Image.new('L', (S, S), 255), mark, wave)

    glow = mark.filter(ImageFilter.GaussianBlur(28)).point(lambda v: v * 0.45)
    out.paste(Image.new('RGBA', (S, S), ACCENT_BOTTOM + (255,)), (0, 0), Image.composite(glow, Image.new('L', (S, S), 0), tile_mask))
    out.paste(gradient(S, ACCENT_TOP, ACCENT_BOTTOM), (0, 0), mark)
    return out


big = render(True)
small = render(False)
sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
frames = [(small if s <= 24 else big).resize((s, s), Image.LANCZOS) for s in sizes]
out_dir = sys.argv[1] if len(sys.argv) > 1 else 'app/assets'
big.resize((512, 512), Image.LANCZOS).save(out_dir + '/icon.png')
frames[-1].save(out_dir + '/bardo.ico', sizes=[(s, s) for s in sizes], append_images=frames[:-1])
print('ok')
