"""COG tile rendering pipeline."""

from __future__ import annotations
import io
import math
from typing import Literal

import numpy as np
import rasterio
from rasterio.crs import CRS
from rasterio.transform import from_bounds
from rasterio.windows import from_bounds as window_from_bounds
from pyproj import Transformer
from PIL import Image

from .geo import tile_to_bbox, best_overview, Bbox
from .style import apply_colormap, stretch_rgb, hillshade


def _reproject_bbox(bbox: Bbox, dst_crs: CRS | None) -> Bbox:
    if dst_crs is None:
        return bbox
    src_crs = CRS.from_epsg(4326)
    if dst_crs == src_crs:
        return bbox
    t = Transformer.from_crs(src_crs, dst_crs, always_xy=True)
    corners = [
        t.transform(bbox.west, bbox.north),
        t.transform(bbox.east, bbox.north),
        t.transform(bbox.west, bbox.south),
        t.transform(bbox.east, bbox.south),
    ]
    xs = [c[0] for c in corners]
    ys = [c[1] for c in corners]
    return Bbox(min(xs), min(ys), max(xs), max(ys))


def render_tile(
    path: str,
    z: int,
    x: int,
    y: int,
    style: Literal["colormap", "rgb", "hillshade"] = "colormap",
    colormap: str = "viridis",
    vmin: float | None = None,
    vmax: float | None = None,
    tile_size: int = 256,
    bilinear: bool = True,
    z_factor: float = 1.0,
    azimuth: float = 315.0,
    altitude: float = 45.0,
) -> bytes:
    """Render a single XYZ tile from a local or HTTP COG. Returns PNG bytes."""
    bbox_wgs84 = tile_to_bbox(z, x, y)

    with rasterio.open(path) as ds:
        native_crs = ds.crs
        n_overviews = ds.overviews(1)
        ov_idx = best_overview(ds.width, ds.height, tile_size, tile_size, len(n_overviews))

    # Re-open at the chosen overview level
    open_kwargs: dict = {}
    if ov_idx > 0:
        open_kwargs["overview_level"] = ov_idx - 1

    with rasterio.open(path, **open_kwargs) as ds:
        bbox_native = _reproject_bbox(bbox_wgs84, ds.crs)

        window = window_from_bounds(
            bbox_native.west, bbox_native.south,
            bbox_native.east, bbox_native.north,
            ds.transform,
        )

        band_count = ds.count
        if style == "rgb" and band_count >= 3:
            read_bands = [1, 2, 3]
        else:
            read_bands = [1]

        resampling = (rasterio.enums.Resampling.bilinear if bilinear
                      else rasterio.enums.Resampling.nearest)
        data = ds.read(
            read_bands,
            window=window,
            out_shape=(len(read_bands), tile_size, tile_size),
            resampling=resampling,
            boundless=True,
            fill_value=np.nan,
        ).astype(np.float32)

        nodata = ds.nodata

        # Replace nodata sentinel with NaN so styles handle it uniformly
        if nodata is not None:
            data[data == nodata] = np.nan

        # Determine min/max from data stats if not given
        if vmin is None or vmax is None:
            valid = data[np.isfinite(data)]
            auto_min = float(valid.min()) if valid.size > 0 else 0.0
            auto_max = float(valid.max()) if valid.size > 0 else 1.0
            vmin = vmin if vmin is not None else auto_min
            vmax = vmax if vmax is not None else auto_max

        if style == "colormap":
            rgba = apply_colormap(data[0], colormap, vmin, vmax, nodata=None)
        elif style == "rgb":
            mins = (vmin, vmin, vmin)
            maxs = (vmax, vmax, vmax)
            rgba = stretch_rgb(data, mins, maxs, nodata=None)
        elif style == "hillshade":
            # Use pixel size at equator as approximate scale
            gt = ds.transform
            pixel_scale = abs(gt.a)
            if ds.crs and ds.crs.is_geographic:
                # Convert degrees to approximate metres
                pixel_scale *= 111_320.0
            hs = hillshade(data[0], pixel_scale, z_factor=z_factor,
                           azimuth=azimuth, altitude=altitude)
            rgba = apply_colormap(hs, colormap, 0.0, 1.0, nodata=None)
        else:
            raise ValueError(f"Unknown style: {style!r}")

    img = Image.fromarray(rgba, mode="RGBA")
    buf = io.BytesIO()
    img.save(buf, format="PNG")
    return buf.getvalue()
