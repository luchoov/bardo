"""Draws the bardo icon: a geometric lowercase "b" whose bowl opens into a sound wave, flat and
monochrome on a near-black rounded tile.

Usage: python tools/make_icon.py [out_dir] [variant]   (default: app/assets; needs Pillow)
Variants: dark (default), light, outline. With "preview", draws all of them side by side.
"""
import math
import sys

from PIL import Image, ImageDraw

S = 1024
INK = (236, 236, 238, 255)
TILE = (14, 14, 16, 255)
EDGE = (44, 44, 50, 255)


def arc_band(draw, cx, cy, r_out, width, start, end, fill, caps=True):
    """A ring segment with round caps."""
    r_mid = r_out - width / 2
    draw.arc((cx - r_out, cy - r_out, cx + r_out, cy + r_out), start, end, fill=fill, width=round(width))
    if caps:
        for angle in (start, end):
            x = cx + r_mid * math.cos(math.radians(angle))
            y = cy + r_mid * math.sin(math.radians(angle))
            draw.ellipse((x - width / 2, y - width / 2, x + width / 2, y + width / 2), fill=fill)


def mark(draw, ink, variant, small):
    """A geometric "b" (a stem and a full circle) with one sound wave around the bowl."""
    stroke = 124 if small else 88
    r_bowl = 214 if small else 200
    wave = 0 if small else 70
    gap = 60
    width = 2 * r_bowl if not wave else r_bowl + r_bowl + gap + wave
    stem_x = (S - width) / 2
    top, cy = (196, 618) if not small else (180, 630)
    cx = stem_x + r_bowl
    # The stem: round top, square foot hidden in the bowl.
    draw.rounded_rectangle((stem_x, top, stem_x + stroke, cy), radius=stroke / 2, fill=ink)
    draw.rectangle((stem_x, top + stroke, stem_x + stroke, cy), fill=ink)
    # The bowl.
    arc_band(draw, cx, cy, r_bowl, stroke, 0, 360, ink, caps=False)
    if wave:
        arc_band(draw, cx, cy, r_bowl + gap + wave, wave, -50, 50, ink)


def render(variant='dark', small=False):
    img = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    inset = 24 if small else 40
    box = (inset, inset, S - inset, S - inset)
    if variant == 'light':
        d.rounded_rectangle(box, radius=220, fill=INK)
        mark(d, TILE, variant, small)
    elif variant == 'outline':
        d.rounded_rectangle(box, radius=220, fill=TILE, outline=INK, width=36)
        mark(d, INK, variant, small)
    else:
        d.rounded_rectangle(box, radius=220, fill=TILE, outline=EDGE, width=8)
        mark(d, INK, variant, small)
    return img


def save(out_dir, variant):
    big, small = render(variant), render(variant, small=True)
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    frames = [(small if s <= 32 else big).resize((s, s), Image.LANCZOS) for s in sizes]
    big.resize((512, 512), Image.LANCZOS).save(out_dir + '/icon.png')
    frames[-1].save(out_dir + '/bardo.ico', sizes=[(s, s) for s in sizes], append_images=frames[:-1])


def preview(out_path):
    variants = ['dark', 'light', 'outline']
    sheet = Image.new('RGBA', (3 * 300, 2 * 300), (0, 0, 0, 0))
    for row, bg in enumerate([(32, 32, 34, 255), (243, 243, 243, 255)]):
        ImageDraw.Draw(sheet).rectangle((0, row * 300, 1200, row * 300 + 300), fill=bg)
        for col, variant in enumerate(variants):
            big, small = render(variant), render(variant, small=True)
            x, y = col * 300 + 20, row * 300 + 20
            sheet.alpha_composite(big.resize((160, 160), Image.LANCZOS), (x, y))
            sheet.alpha_composite(big.resize((64, 64), Image.LANCZOS), (x + 172, y))
            sheet.alpha_composite(small.resize((32, 32), Image.LANCZOS), (x + 172, y + 80))
            sheet.alpha_composite(small.resize((16, 16), Image.LANCZOS), (x + 214, y + 88))
            d = ImageDraw.Draw(sheet)
            d.text((x, y + 200), variant, fill=(128, 128, 128, 255))
    sheet.save(out_path)


if __name__ == '__main__':
    out_dir = sys.argv[1] if len(sys.argv) > 1 else 'app/assets'
    variant = sys.argv[2] if len(sys.argv) > 2 else 'dark'
    if variant == 'preview':
        preview(out_dir + '/icon-preview.png')
    else:
        save(out_dir, variant)
    print('ok')
