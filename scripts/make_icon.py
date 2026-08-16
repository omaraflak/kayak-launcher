"""Generates the launcher's source app icon.

Tauri's icon generator needs one large square PNG to derive every platform
format from, and the project has no image toolchain available, so the icon is
drawn here analytically and written as a raw PNG through zlib. Shapes are
defined as signed-distance functions in normalised coordinates and sampled with
supersampling, which keeps the edges smooth at every size Tauri downscales to.
"""

import math
import struct
import sys
import zlib
from pathlib import Path

SIZE = 1024
#: Samples per axis. Antialiasing here is what keeps the 16px favicon legible.
SUPERSAMPLE = 3

# Google Blue, matching --md-sys-color-primary in the Kayak frontend.
BACKGROUND = (26, 115, 232)
FOREGROUND = (255, 255, 255)

# Rounded-square plate. macOS and Windows both render the icon unmasked, so the
# icon has to supply its own corner radius and breathing room.
PLATE_INSET = 0.055
PLATE_RADIUS = 0.235

# Kayak hull, drawn as the lens where two circles overlap. Solving for a hull of
# half-width HULL_W and half-length HULL_L gives the circle radius and offset.
HULL_W = 0.090
HULL_L = 0.370
HULL_ANGLE = math.radians(-35.0)

# Paddle laid across the hull, with a knocked-out gap so the two shapes read as
# separate silhouettes rather than merging into one blob.
#
# The hull's long axis starts vertical and the paddle's starts horizontal, so
# giving both the same rotation is what keeps them perpendicular. Rotating them
# by different amounts leaves the two shapes nearly parallel, which reads as a
# few unrelated slivers rather than a boat and a paddle.
PADDLE_ANGLE = HULL_ANGLE
PADDLE_HALF_LENGTH = 0.330
PADDLE_RADIUS = 0.024
PADDLE_GAP = 0.026


def _rounded_rect(x: float, y: float, half: float, radius: float) -> float:
    """Signed distance to a rounded square centred on the origin."""
    dx = abs(x) - (half - radius)
    dy = abs(y) - (half - radius)
    outside = math.hypot(max(dx, 0.0), max(dy, 0.0))
    return outside + min(max(dx, dy), 0.0) - radius


def _rotate(x: float, y: float, angle: float) -> tuple[float, float]:
    cos, sin = math.cos(angle), math.sin(angle)
    return x * cos + y * sin, -x * sin + y * cos


def _hull(x: float, y: float) -> float:
    """Signed distance to the kayak hull.

    The hull is the intersection of two circles of radius `radius` centred at
    (+/-offset, 0), which produces a symmetric pointed oval. Intersection of two
    fields is their maximum.
    """
    radius = (HULL_L**2 + HULL_W**2) / (2.0 * HULL_W)
    offset = radius - HULL_W
    local_x, local_y = _rotate(x, y, HULL_ANGLE)
    left = math.hypot(local_x - offset, local_y) - radius
    right = math.hypot(local_x + offset, local_y) - radius
    return max(left, right)


def _paddle(x: float, y: float) -> float:
    """Signed distance to the paddle, a capsule through the origin."""
    local_x, local_y = _rotate(x, y, PADDLE_ANGLE)
    clamped = max(-PADDLE_HALF_LENGTH, min(PADDLE_HALF_LENGTH, local_x))
    return math.hypot(local_x - clamped, local_y) - PADDLE_RADIUS


def _sample(x: float, y: float) -> tuple[int, int, int, int]:
    """Resolves one point to RGBA by layering plate, paddle, gap, then hull.

    The hull is drawn last, on top of the paddle, so the boat stays one
    continuous silhouette. Drawing the paddle on top instead cuts the hull into
    two wedges, which reads as a bowtie rather than a kayak.
    """
    if _rounded_rect(x, y, 0.5 - PLATE_INSET, PLATE_RADIUS) > 0.0:
        return (0, 0, 0, 0)

    hull = _hull(x, y)
    if hull <= 0.0:
        return (*FOREGROUND, 255)
    # A gap knocked out around the hull keeps the paddle from merging into it.
    if hull <= PADDLE_GAP:
        return (*BACKGROUND, 255)
    if _paddle(x, y) <= 0.0:
        return (*FOREGROUND, 255)
    return (*BACKGROUND, 255)


def _render(size: int) -> bytes:
    """Renders the icon to raw RGBA scanlines, each prefixed with a filter byte."""
    rows = bytearray()
    step = 1.0 / (size * SUPERSAMPLE)
    weight = SUPERSAMPLE * SUPERSAMPLE

    for py in range(size):
        rows.append(0)
        for px in range(size):
            red = green = blue = alpha = 0
            for sy in range(SUPERSAMPLE):
                y = (py * SUPERSAMPLE + sy + 0.5) * step - 0.5
                for sx in range(SUPERSAMPLE):
                    x = (px * SUPERSAMPLE + sx + 0.5) * step - 0.5
                    sr, sg, sb, sa = _sample(x, y)
                    # Premultiply so transparent samples do not lighten the edge.
                    red += sr * sa
                    green += sg * sa
                    blue += sb * sa
                    alpha += sa
            if alpha == 0:
                rows.extend((0, 0, 0, 0))
            else:
                rows.extend(
                    (red // alpha, green // alpha, blue // alpha, alpha // weight)
                )
    return bytes(rows)


def _chunk(tag: bytes, payload: bytes) -> bytes:
    return (
        struct.pack(">I", len(payload))
        + tag
        + payload
        + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
    )


def write_png(path: Path, size: int) -> None:
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", header)
        + _chunk(b"IDAT", zlib.compress(_render(size), 9))
        + _chunk(b"IEND", b"")
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(png)


if __name__ == "__main__":
    target = Path(sys.argv[1] if len(sys.argv) > 1 else "app-icon.png")
    write_png(target, SIZE)
    print(f"wrote {target} ({SIZE}x{SIZE})")
