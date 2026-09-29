"""Zonal statistics over polygons against a COG."""

from __future__ import annotations
import numpy as np
import rasterio
import rasterio.mask
from pyproj import Transformer
from shapely import wkt as shapely_wkt
from shapely.ops import transform as shapely_transform
from rasterio.crs import CRS


def zonal_stats(
    path: str,
    polygons_wkt: list[str],
    band: int = 1,
    nodata: float | None = None,
) -> list[dict]:
    """Compute min/max/mean/stdev for each WGS84 WKT polygon over a COG band.

    Returns a list of dicts with keys: min, max, mean, stdev, count.
    Invalid/empty polygons return a dict with None values.
    """
    results: list[dict] = []

    with rasterio.open(path) as ds:
        native_crs = ds.crs
        file_nodata = ds.nodata if nodata is None else nodata

        src_crs = CRS.from_epsg(4326)
        need_reproject = native_crs is not None and native_crs != src_crs

        if need_reproject:
            transformer = Transformer.from_crs(src_crs, native_crs, always_xy=True)

        for wkt_str in polygons_wkt:
            try:
                geom = shapely_wkt.loads(wkt_str)

                if need_reproject:
                    geom = shapely_transform(transformer.transform, geom)

                masked, _ = rasterio.mask.mask(
                    ds, [geom], crop=True, indexes=[band],
                    nodata=np.nan if file_nodata is None else file_nodata,
                    filled=True,
                )
                values = masked[0].astype(np.float32)

                # Build invalid mask
                invalid = ~np.isfinite(values)
                if file_nodata is not None:
                    nd = float(file_nodata)
                    tol = max(1e-5, abs(nd) * 1e-5)
                    invalid |= np.abs(values - nd) <= tol

                valid = values[~invalid]
                if valid.size == 0:
                    results.append({"min": None, "max": None, "mean": None,
                                    "stdev": None, "count": 0})
                    continue

                mean = float(np.mean(valid))
                results.append({
                    "min": float(np.min(valid)),
                    "max": float(np.max(valid)),
                    "mean": mean,
                    "stdev": float(np.std(valid, ddof=0)),
                    "count": int(valid.size),
                })
            except Exception:
                results.append({"min": None, "max": None, "mean": None,
                                "stdev": None, "count": 0})

    return results
