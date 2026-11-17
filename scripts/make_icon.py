"""Generate the app icon: a small sunburst on a dark rounded square."""

from __future__ import annotations

import math
import random
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
SIZE = 1024

PALETTE = [
    (124, 158, 255),
    (94, 210, 168),
    (247, 191, 92),
    (239, 118, 122),
    (176, 140, 255),
    (96, 196, 232),
    (214, 232, 120),
]

# Deterministic, so re-running produces the same icon.
random.seed(7)


def rounded_background(size: int) -> Image.Image:
    base = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(base)
    radius = int(size * 0.22)
    draw.rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=radius, fill=(14, 17, 23, 255)
    )
    # Soft inner glow.
    glow = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    glow_draw = ImageDraw.Draw(glow)
    for step in range(24, 0, -1):
        alpha = int(3.2 * step)
        inset = int(size * 0.02 * (24 - step))
        glow_draw.ellipse(
            (inset, inset, size - 1 - inset, size - 1 - inset),
            fill=(40, 48, 66, alpha),
        )
    return Image.alpha_composite(base, glow)


def sunburst(size: int) -> Image.Image:
    image = Image.new("RGBA", (size * 2, size * 2), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    big = size * 2.0
    cx = cy = big / 2

    inner = big * 0.145
    rings = [(inner, big * 0.30, 9), (big * 0.315, big * 0.435, 13)]

    for ring_index, (r0, r1, count) in enumerate(rings):
        start = random.uniform(0, math.tau)
        weights = [random.uniform(0.6, 2.4) for _ in range(count)]
        total = sum(weights)
        angle = start
        for index, weight in enumerate(weights):
            span = math.tau * (weight / total)
            color = PALETTE[index % len(PALETTE)]
            shade = 0.72 + 0.35 * ((index * 7 + ring_index * 3) % 5) / 5
            fill = tuple(min(255, int(channel * shade)) for channel in color)
            draw.pieslice(
                (cx - r1, cy - r1, cx + r1, cy + r1),
                start=math.degrees(angle) + 1.2,
                end=math.degrees(angle + span) - 1.2,
                fill=fill + (255,),
            )
            angle += span

    # Centre disc.
    draw.ellipse(
        (cx - inner, cy - inner, cx + inner, cy + inner),
        fill=(29, 35, 49, 255),
        outline=(70, 82, 110, 255),
        width=max(2, int(size * 0.01)),
    )
    return image.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    icon = sunburst(SIZE)
    canvas = Image.alpha_composite(rounded_background(SIZE), icon)

    for dimension in (32, 64, 128, 256, 512, 1024):
        resized = canvas.resize((dimension, dimension), Image.LANCZOS)
        name = f"{dimension}x{dimension}.png"
        resized.save(OUT / name)
        print("wrote", name)

    canvas.resize((256, 256), Image.LANCZOS).save(
        OUT / "icon.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("wrote icon.ico")


if __name__ == "__main__":
    main()
