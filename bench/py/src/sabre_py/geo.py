"""Geospatial coordinate math — tile↔bbox, pixel windows, overview selection."""

from __future__ import annotations
import math
from typing import NamedTuple


class Bbox(NamedTuple):
    west: float
    south: float
    east: float
    north: float


class PixelWindow(NamedTuple):
    x_off: int
    y_off: int
    x_size: int
    y_size: int


def tile_to_bbox(z: int, x: int, y: int) -> Bbox:
    """Convert Web Mercator tile indices to a WGS84 bounding box."""
    n = 2 ** z
    west = x / n * 360.0 - 180.0
    east = (x + 1) / n * 360.0 - 180.0
    north_rad = math.atan(math.sinh(math.pi * (1 - 2 * y / n)))
    south_rad = math.atan(math.sinh(math.pi * (1 - 2 * (y + 1) / n)))
    return Bbox(west=west, south=math.degrees(south_rad), east=east, north=math.degrees(north_rad))


def best_overview(full_w: int, full_h: int, out_w: int, out_h: int, n_overviews: int) -> int:
    """Return the best overview index (0 = full res, 1 = 2× down, …)."""
    target = min(full_w / out_w, full_h / out_h)
    best = 0
    for i in range(n_overviews + 1):
        if 2 ** i <= target:
            best = i
        else:
            break
    return best


def bbox_to_pixel_window(
    gt: tuple[float, float, float, float, float, float],
    img_w: int,
    img_h: int,
    bbox: Bbox,
) -> PixelWindow | None:
    """Convert a geographic bbox to a pixel window using affine inversion.

    gt = (x_origin, pixel_width, x_rotation, y_origin, y_rotation, pixel_height)
    pixel_height is typically negative.
    """
    x0, pw, xr, y0, yr, ph = gt
    det = pw * ph - xr * yr
    if abs(det) < 1e-15:
        return None

    inv_pw = ph / det
    inv_xr = -xr / det
    inv_yr = -yr / det
    inv_ph = pw / det

    cols = []
    rows = []
    for gx, gy in [(bbox.west, bbox.north), (bbox.east, bbox.north),
                   (bbox.west, bbox.south), (bbox.east, bbox.south)]:
        dx = gx - x0
        dy = gy - y0
        cols.append(inv_pw * dx + inv_xr * dy)
        rows.append(inv_yr * dx + inv_ph * dy)

    c0 = max(0, int(math.floor(min(cols))))
    r0 = max(0, int(math.floor(min(rows))))
    c1 = min(img_w, int(math.ceil(max(cols))))
    r1 = min(img_h, int(math.ceil(max(rows))))

    if c1 <= c0 or r1 <= r0:
        return None
    return PixelWindow(x_off=c0, y_off=r0, x_size=c1 - c0, y_size=r1 - r0)
