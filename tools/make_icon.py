"""Draws the bardo icon: a lowercase "b" with a sound wave in its bowl, flat and monochrome on a
near-black rounded tile.

Usage: python tools/make_icon.py [out_dir] [variant]   (default: app/assets; needs Pillow)
Variants: dark (default), light, outline. With "preview", draws all of them side by side.
"""
import sys

from PIL import Image, ImageDraw

S = 1024
INK = (236, 236, 238, 255)
TILE = (14, 14, 16, 255)
EDGE = (44, 44, 50, 255)
# The wave is a step quieter than the letter.
WAVE = (196, 196, 200, 255)
WAVE_ON_LIGHT = (92, 92, 98, 255)


# The mark, drawn on a 1254 px grid and scaled into the tile: the stem's left edge is the bowl's.
SCALE = 0.87
CENTER = (646, 635)
STEM = (387, 523, 290)  # left, right, top
STEM_CORNER = 92  # the top right corner, rounded like a flag; the top left is nearly square
BOWL = (646, 722, 259)  # centre and outer radius
# Bars of a sound wave inside the bowl: centre x, height. Few and wide at icon sizes, where five
# thin ones would blur into one.
BARS = [(533, 64), (589, 128), (645, 210), (701, 128), (757, 64)]
BARS_SMALL = [(575, 96), (645, 190), (715, 96)]


def at(x, y):
    return (x - CENTER[0]) * SCALE + S / 2, (y - CENTER[1]) * SCALE + S / 2


def mark(small):
    """Masks for a lowercase "b" with a sound wave in its bowl: the letter, and the bars."""
    letter = Image.new('L', (S, S), 0)
    d = ImageDraw.Draw(letter)
    cx, cy, r = BOWL
    hole = 132 if small else 163
    (x0, y0), (x1, y1) = at(cx - r, cy - r), at(cx + r, cy + r)
    d.ellipse((x0, y0, x1, y1), fill=255)
    left, right, top = STEM
    (x0, y0), (x1, y1) = at(left, top), at(right, cy)
    d.rounded_rectangle((x0, y0, x1, y1), radius=14 * SCALE, fill=255)
    corner = STEM_CORNER * SCALE
    d.rectangle((x1 - corner, y0 - 2, x1 + 2, y0 + corner), fill=0)
    d.pieslice((x1 - 2 * corner, y0, x1, y0 + 2 * corner), 270, 360, fill=255)
    (x0, y0), (x1, y1) = at(cx - 5 - hole, cy - hole), at(cx - 5 + hole, cy + hole)
    d.ellipse((x0, y0, x1, y1), fill=0)

    bars = Image.new('L', (S, S), 0)
    d = ImageDraw.Draw(bars)
    width = 40 if small else 30
    for x, height in BARS_SMALL if small else BARS:
        (x0, y0), (x1, y1) = at(x - width / 2, 729 - height / 2), at(x + width / 2, 729 + height / 2)
        d.rounded_rectangle((x0, y0, x1, y1), radius=width * SCALE / 2, fill=255)
    return letter, bars


def render(variant='dark', small=False):
    img = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    inset = 24 if small else 40
    box = (inset, inset, S - inset, S - inset)
    if variant == 'light':
        d.rounded_rectangle(box, radius=220, fill=INK)
        ink, wave = TILE, WAVE_ON_LIGHT
    elif variant == 'outline':
        d.rounded_rectangle(box, radius=220, fill=TILE, outline=INK, width=36)
        ink, wave = INK, WAVE
    else:
        d.rounded_rectangle(box, radius=220, fill=TILE, outline=EDGE, width=8)
        ink, wave = INK, WAVE
    letter, bars = mark(small)
    img.paste(ink, mask=letter)
    img.paste(wave, mask=bars)
    return img


def with_badge(img):
    """The tray icon while muted or deafened: a red dot in the corner, cut out of the tile."""
    img = img.copy()
    d = ImageDraw.Draw(img)
    cx = cy = S - 230
    r = 200
    d.ellipse((cx - r - 44, cy - r - 44, cx + r + 44, cy + r + 44), fill=(0, 0, 0, 0))
    d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=(240, 86, 91, 255))
    return img


def save(out_dir, variant):
    big, small = render(variant), render(variant, small=True)
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    frames = [(small if s <= 32 else big).resize((s, s), Image.LANCZOS) for s in sizes]
    big.resize((512, 512), Image.LANCZOS).save(out_dir + '/icon.png')
    with_badge(small).resize((64, 64), Image.LANCZOS).save(out_dir + '/tray-muted.png')
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
