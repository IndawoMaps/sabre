"""Raster styling: colormap, RGB stretch, hillshade."""

from __future__ import annotations
import math
import numpy as np
from .colormaps import get_lut

_NODATA_EPS = 1e-5


def _nodata_mask(values: np.ndarray, nodata: float | None) -> np.ndarray:
    mask = ~np.isfinite(values)
    if nodata is not None:
        nd = float(nodata)
        tol = max(_NODATA_EPS, abs(nd) * _NODATA_EPS)
        mask |= np.abs(values - nd) <= tol
    return mask


def apply_colormap(
    values: np.ndarray,
    colormap: str,
    vmin: float,
    vmax: float,
    nodata: float | None = None,
) -> np.ndarray:
    """Map a (H, W) float32 array to (H, W, 4) RGBA uint8."""
    lut = get_lut(colormap)
    bad = _nodata_mask(values, nodata)

    span = vmax - vmin
    if span == 0:
        t = np.zeros_like(values, dtype=np.float32)
    else:
        t = np.clip((values.astype(np.float32) - vmin) / span, 0.0, 1.0)

    t_safe = np.where(bad, 0.0, t)
    idx = np.round(t_safe * 255.0).astype(np.uint8)

    rgba = np.empty((*values.shape, 4), dtype=np.uint8)
    rgba[..., :3] = lut[idx]
    rgba[..., 3] = np.where(bad, 0, 255).astype(np.uint8)
    rgba[bad] = 0
    return rgba


def stretch_rgb(
    data: np.ndarray,
    mins: tuple[float, float, float],
    maxs: tuple[float, float, float],
    nodata: float | None = None,
) -> np.ndarray:
    """Map (3, H, W) float32 to (H, W, 4) RGBA uint8 with per-channel stretch."""
    h, w = data.shape[1], data.shape[2]
    rgba = np.zeros((h, w, 4), dtype=np.uint8)

    bad = _nodata_mask(data[0], nodata)
    for ch in range(3):
        span = maxs[ch] - mins[ch]
        if span == 0:
            stretched = np.zeros((h, w), dtype=np.uint8)
        else:
            t = np.clip((data[ch].astype(np.float32) - mins[ch]) / span, 0.0, 1.0)
            stretched = np.round(t * 255).astype(np.uint8)
        rgba[..., ch] = stretched

    rgba[..., 3] = np.where(bad, 0, 255).astype(np.uint8)
    rgba[bad] = 0
    return rgba


def hillshade(
    elev: np.ndarray,
    pixel_scale: float,
    z_factor: float = 1.0,
    azimuth: float = 315.0,
    altitude: float = 45.0,
) -> np.ndarray:
    """Compute hillshade from elevation raster. Returns (H, W) float32 in [0, 1]."""
    h, w = elev.shape
    out = np.zeros((h, w), dtype=np.float32)

    az_rad = math.radians(360.0 - azimuth + 90.0)
    zenith_rad = math.radians(90.0 - altitude)

    e = elev.astype(np.float64)
    denom = 8.0 * pixel_scale * z_factor

    # Sobel-style central differences on interior pixels
    dz_dx = (
        e[0:h-2, 2:w] - e[0:h-2, 0:w-2]
        + 2 * e[1:h-1, 2:w] - 2 * e[1:h-1, 0:w-2]
        + e[2:h,   2:w] - e[2:h,   0:w-2]
    ) / denom

    dz_dy = (
        e[2:h,   0:w-2] - e[0:h-2, 0:w-2]
        + 2 * e[2:h,   1:w-1] - 2 * e[0:h-2, 1:w-1]
        + e[2:h,   2:w] - e[0:h-2, 2:w]
    ) / denom

    slope = np.arctan(np.sqrt(dz_dx ** 2 + dz_dy ** 2))
    aspect = np.arctan2(dz_dy, -dz_dx)
    aspect = np.where(aspect < 0, aspect + 2 * math.pi, aspect)

    hs = (
        math.cos(zenith_rad) * np.cos(slope)
        + math.sin(zenith_rad) * np.sin(slope) * np.cos(az_rad - aspect)
    )
    out[1:h-1, 1:w-1] = np.maximum(hs, 0).astype(np.float32)
    return out
