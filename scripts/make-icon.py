"""Draw the Disk Scanner icon and write `assets/icon.ico` + `assets/icon.png`.

The mark is the app's own chart: concentric arcs (a sunburst) around an empty
hub, on the same dark card the window uses.
"""

from __future__ import annotations

import io
from pathlib import Path

from PIL import Image, ImageDraw

SIZE = 1024
ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "apps" / "desktop-gpui" / "assets"

BACKDROP_TOP = (18, 24, 35, 255)
BACKDROP_BOTTOM = (9, 13, 20, 255)
HUB = (12, 17, 25, 255)
RING_COLORS = [
    (96, 156, 255, 255),   # blue   — the biggest slice
    (86, 200, 176, 255),   # teal
    (232, 161, 58, 255),   # amber  — a file sliver
    (150, 120, 235, 255),  # violet
    (60, 110, 190, 255),   # deep blue
]

# (ring share of the used radius, slices as (start fraction, end fraction))
RINGS = [
    (0.00, 0.34, [(0.02, 0.46), (0.47, 0.63), (0.64, 0.80), (0.81, 0.99)]),
    (0.36, 0.62, [(0.02, 0.28), (0.30, 0.44), (0.45, 0.62), (0.63, 0.80), (0.82, 0.99)]),
    (0.64, 0.82, [(0.02, 0.20), (0.22, 0.40), (0.42, 0.60), (0.62, 0.80), (0.82, 0.99)]),
    (0.84, 0.94, [(0.02, 0.30), (0.32, 0.55), (0.57, 0.73), (0.75, 0.99)]),
]

# Two fat rings instead of four thin ones: below ~48 px the detail above
# collapses into an indistinct donut, so small sizes get their own drawing.
RINGS_SIMPLE = [
    (0.16, 0.48, [(0.02, 0.40), (0.45, 0.64), (0.69, 0.98)]),
    (0.53, 0.92, [(0.02, 0.32), (0.37, 0.60), (0.65, 0.98)]),
]


def vertical_gradient(size: int, top, bottom) -> Image.Image:
    grad = Image.new("RGBA", (1, size))
    for y in range(size):
        t = y / max(1, size - 1)
        grad.putpixel(
            (0, y),
            tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(4)),
        )
    return grad.resize((size, size))


def rounded_mask(size: int, radius: int) -> Image.Image:
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size - 1, size - 1), radius, fill=255)
    return mask


def annulus_slice_mask(size: int, centre: float, outer: float, inner: float,
                       start_deg: float, end_deg: float) -> Image.Image:
    """Opaque wedge between two radii; drawn on an L mask so holes really cut."""
    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    draw.pieslice(
        (centre - outer, centre - outer, centre + outer, centre + outer),
        start_deg,
        end_deg,
        fill=255,
    )
    if inner > 0.5:
        draw.ellipse(
            (centre - inner, centre - inner, centre + inner, centre + inner),
            fill=0,
        )
    return mask


def make_master(rings=RINGS) -> Image.Image:
    # Everything is drawn at 2x and downscaled, so the arc edges are smooth.
    scale = 2
    size = SIZE * scale
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    plate = vertical_gradient(size, BACKDROP_TOP, BACKDROP_BOTTOM)
    canvas.paste(plate, (0, 0), rounded_mask(size, int(size * 0.22)))

    ctx = ImageDraw.Draw(canvas, "RGBA")
    centre = size / 2
    radius = size * 0.40
    hub = size * 0.115

    # Soft halo behind the chart, so the arcs read against the card.
    ctx.ellipse(
        (centre - radius * 1.16, centre - radius * 1.16,
         centre + radius * 1.16, centre + radius * 1.16),
        fill=(40, 70, 120, 34),
    )

    for ring_index, (inner, outer, slices) in enumerate(rings):
        for slice_index, (start, end) in enumerate(slices):
            colour = RING_COLORS[(ring_index + slice_index) % len(RING_COLORS)]
            # 12 o'clock start, clockwise, small gaps between slices.
            mask = annulus_slice_mask(
                size,
                centre,
                radius * outer,
                radius * inner,
                -90 + 360 * start,
                -90 + 360 * end,
            )
            canvas.paste(Image.new("RGBA", (size, size), colour), (0, 0), mask)

    ctx.ellipse(
        (centre - hub, centre - hub, centre + hub, centre + hub),
        fill=HUB,
    )
    # A single bright tick in the hub, like the cursor line of the chart.
    ctx.line(
        (centre - hub * 0.55, centre, centre + hub * 0.55, centre),
        fill=RING_COLORS[0],
        width=max(2, int(size * 0.012)),
    )
    return canvas.resize((SIZE, SIZE), Image.LANCZOS)


def write_ico(path: Path, frames: list[Image.Image]) -> None:
    """Write a multi-image .ico, keeping each frame exactly as rendered.

    Pillow always rebuilds every size from a single base image, which would
    throw away the simplified small-size drawings, so the container is written
    by hand: one PNG-compressed entry per frame (supported since Vista).
    """
    import struct

    payloads = []
    for frame in frames:
        buffer = io.BytesIO()
        frame.save(buffer, format="PNG")
        payloads.append(buffer.getvalue())

    header = struct.pack("<HHH", 0, 1, len(frames))
    offset = len(header) + 16 * len(frames)
    directory = bytearray()
    for frame, payload in zip(frames, payloads):
        size = frame.width
        directory += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0,
            0,
            1,
            32,
            len(payload),
            offset,
        )
        offset += len(payload)

    path.write_bytes(header + bytes(directory) + b"".join(payloads))


def main() -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)
    master = make_master()
    simple = make_master(RINGS_SIMPLE)

    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [
        (simple if size < 48 else master).resize((size, size), Image.LANCZOS)
        for size in sizes
    ]
    frames[-1].save(ASSETS / "icon.png")
    write_ico(ASSETS / "icon.ico", frames)
    print("wrote", ASSETS / "icon.ico", "and", ASSETS / "icon.png")


if __name__ == "__main__":
    main()
