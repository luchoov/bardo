"""Builds the README animation from the frames tools/screenshots.ps1 records.

    python tools/make_gif.py <frames dir> <out.gif>

The frames directory holds 0000.png, 0001.png... and times.txt, the milliseconds at which each one
was taken. Repeated frames are merged into one that stays up longer, which keeps the file small.
"""

import sys
from pathlib import Path

from PIL import Image

frames_dir, out = Path(sys.argv[1]), Path(sys.argv[2])
times = [int(line) for line in (frames_dir / "times.txt").read_text().split()]
paths = sorted(frames_dir.glob("*.png"))

frames, durations = [], []
for index, path in enumerate(paths):
    frame = Image.open(path).convert("RGB")
    until = times[index + 1] if index + 1 < len(times) else times[index] + 1000
    duration = until - times[index]
    if frames and frame.tobytes() == frames[-1].tobytes():
        durations[-1] += duration
    else:
        frames.append(frame)
        durations.append(duration)

# One palette for every frame, so the dark background does not flicker between frames.
palette = frames[0].quantize(colors=128, method=Image.Quantize.MEDIANCUT)
frames = [frame.quantize(palette=palette, dither=Image.Dither.NONE) for frame in frames]
frames[0].save(out, save_all=True, append_images=frames[1:], duration=durations, loop=0, optimize=True)
print(f"{out}: {len(frames)} frames, {sum(durations) / 1000:.1f} s, {out.stat().st_size // 1024} KB")
