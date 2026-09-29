"""
Python port of crates/core/tests/snapshot.rs.

The same synthetic GeoTIFFs (l_shape, o_shape) are built in Python and rendered
with the same parameters used in the Rust tests.  Two test levels:

  Sanity tests  — exact structural invariants (size, opaque pixel count,
                  transparency at known positions) that must hold regardless of
                  implementation differences.

  Snapshot tests — pixel comparison against the Rust-generated golden PNGs in
                   crates/core/tests/fixtures/.  We allow a small per-pixel tolerance
                   (≤ 2 per channel) because rasterio's nearest-neighbour
                   resampling may place boundary pixels 1 step differently from
                   the custom Rust implementation.
"""

from __future__ import annotations
import io
import os
import struct
import tempfile

import numpy as np
import pytest
from PIL import Image

# ── Resolve path to Rust fixtures ─────────────────────────────────────────────

_HERE = os.path.dirname(__file__)
FIXTURES_DIR = os.path.abspath(os.path.join(_HERE, "../../../crates/core/tests/fixtures"))

# ── Synthetic GeoTIFF builder (mirrors make_geotiff in snapshot.rs) ───────────
#
# Minimal single-band float32 GeoTIFF: little-endian, no compression, one strip.
# No EPSG tag → render pipeline treats coordinates as WGS84 passthrough.
#
# Layout:
#   0   header (8)
#   8   IFD count (2) + 14 entries×12 (168) + next_ifd=0 (4)  →  total 174
#   182 ModelPixelScaleTag  3×f64=24
#   206 ModelTiepointTag    6×f64=48
#   254 pixel data          width×height×4 bytes (float32 LE)

def make_geotiff(
    pixels: list[float],
    width: int,
    height: int,
    west: float,
    north: float,
    pixel_size: float,
    nodata: float,
) -> bytes:
    assert len(pixels) == width * height

    SCALE_OFFSET = 182
    TIEPOINT_OFFSET = 206
    DATA_OFFSET = 254
    pixel_data_len = width * height * 4

    # GDAL_NODATA stored as ASCII (must fit ≤ 4 bytes inline, same as Rust assert)
    nodata_str = f"{nodata:g}\x00"
    nd_bytes = nodata_str.encode("ascii")
    assert len(nd_bytes) <= 4, "nodata string too long for inline storage"
    nd_inline = nd_bytes.ljust(4, b"\x00")
    nd_count = len(nd_bytes)

    def entry(tag: int, typ: int, count: int, val: bytes) -> bytes:
        return struct.pack("<HHI", tag, typ, count) + val

    def u16v(v: int) -> bytes:
        return struct.pack("<H2x", v)

    def u32v(v: int) -> bytes:
        return struct.pack("<I", v)

    buf = bytearray()

    # Header: little-endian magic, TIFF version 42, IFD offset = 8
    buf += b"II"
    buf += struct.pack("<H", 42)
    buf += struct.pack("<I", 8)

    # IFD — 14 entries in ascending tag order
    buf += struct.pack("<H", 14)
    buf += entry(256,   4, 1,         u32v(width))            # ImageWidth
    buf += entry(257,   4, 1,         u32v(height))           # ImageHeight
    buf += entry(258,   3, 1,         u16v(32))               # BitsPerSample
    buf += entry(259,   3, 1,         u16v(1))                # Compression (none)
    buf += entry(262,   3, 1,         u16v(1))                # PhotometricInterp
    buf += entry(273,   4, 1,         u32v(DATA_OFFSET))      # StripOffsets
    buf += entry(277,   3, 1,         u16v(1))                # SamplesPerPixel
    buf += entry(278,   4, 1,         u32v(height))           # RowsPerStrip
    buf += entry(279,   4, 1,         u32v(pixel_data_len))   # StripByteCounts
    buf += entry(284,   3, 1,         u16v(1))                # PlanarConfiguration
    buf += entry(339,   3, 1,         u16v(3))                # SampleFormat (float32)
    buf += entry(33550, 12, 3,        u32v(SCALE_OFFSET))     # ModelPixelScaleTag
    buf += entry(33922, 12, 6,        u32v(TIEPOINT_OFFSET))  # ModelTiepointTag
    buf += entry(42113, 2,  nd_count, nd_inline)              # GDAL_NODATA
    buf += struct.pack("<I", 0)                               # next IFD = 0

    assert len(buf) == 182, f"IFD block is {len(buf)} bytes, expected 182"

    # ModelPixelScaleTag: [scale_x, scale_y, 0.0]
    buf += struct.pack("<3d", pixel_size, pixel_size, 0.0)

    # ModelTiepointTag: pixel(0,0,0) → geo(west, north, 0)
    buf += struct.pack("<6d", 0.0, 0.0, 0.0, west, north, 0.0)

    assert len(buf) == 254, f"Pre-data block is {len(buf)} bytes, expected 254"

    for p in pixels:
        buf += struct.pack("<f", p)

    return bytes(buf)


# ── Shape pixel generators (mirrors snapshot.rs) ──────────────────────────────
#
# Each shape is a 25×25 grid (1 km/pixel, anchored at west=10°, north=0.225°,
# pixel_size=0.009°/px).  Background = 0.0 (nodata → transparent).

def l_shape() -> list[float]:
    """Vertical stroke cols 0–7 + horizontal stroke rows 17–24.
    Value = 1 + 9 × (row / 24)  →  blue at top, yellow at bottom (viridis)."""
    px = [0.0] * (25 * 25)
    for row in range(25):
        for col in range(25):
            if col < 8 or row >= 17:
                px[row * 25 + col] = 1.0 + 9.0 * row / 24.0
    return px


def o_shape() -> list[float]:
    """Ring spanning rows 4–20, cols 4–20, border ≤ 2 px thick.
    Value = 1 + 9 × ((col − 4) / 16)  →  blue on left, yellow on right."""
    px = [0.0] * (25 * 25)
    for row in range(4, 21):
        for col in range(4, 21):
            if row <= 6 or row >= 18 or col <= 6 or col >= 18:
                px[row * 25 + col] = 1.0 + 9.0 * (col - 4) / 16.0
    return px


# ── Render helper ─────────────────────────────────────────────────────────────
#
# Matches the Rust test: tile z=9, x=270, y=255 covers ~9.84°–10.55°E, 0°–0.35°N,
# fully containing our 25×25 TIFF (10°–10.225°E, 0°–0.225°N).

TILE_Z, TILE_X, TILE_Y = 9, 270, 255


def render_shape(pixels: list[float]) -> np.ndarray:
    """Render the given pixel grid as a 256×256 RGBA tile array.

    Uses the same parameters as the Rust test:
    - viridis colormap, vmin=1.0, vmax=10.0, nodata=0.0
    - nearest-neighbour resampling (bilinear=False)
    """
    from sabre_py.render import render_tile

    tiff_bytes = make_geotiff(pixels, 25, 25, 10.0, 0.225, 0.009, 0.0)

    with tempfile.NamedTemporaryFile(suffix=".tif", delete=False) as f:
        f.write(tiff_bytes)
        tmp_path = f.name

    try:
        png = render_tile(
            tmp_path,
            z=TILE_Z, x=TILE_X, y=TILE_Y,
            style="colormap",
            colormap="viridis",
            vmin=1.0,
            vmax=10.0,
            tile_size=256,
            bilinear=False,
        )
    finally:
        os.unlink(tmp_path)

    img = Image.open(io.BytesIO(png)).convert("RGBA")
    return np.array(img)


def _load_fixture_rgba(name: str) -> np.ndarray:
    path = os.path.join(FIXTURES_DIR, name)
    if not os.path.exists(path):
        pytest.skip(f"Fixture not found: {path}")
    img = Image.open(path).convert("RGBA")
    return np.array(img)


# ── Sanity tests (mirror l_shape_has_opaque_pixels_in_expected_region) ────────

class TestSanity:
    def test_l_shape_output_size(self):
        rgba = render_shape(l_shape())
        assert rgba.shape == (256, 256, 4), f"Expected (256,256,4), got {rgba.shape}"

    def test_o_shape_output_size(self):
        rgba = render_shape(o_shape())
        assert rgba.shape == (256, 256, 4), f"Expected (256,256,4), got {rgba.shape}"

    def test_l_shape_opaque_pixel_count(self):
        """Rendered L-shape must contain thousands of opaque pixels."""
        rgba = render_shape(l_shape())
        opaque = int((rgba[:, :, 3] == 255).sum())
        assert opaque > 1000, f"Expected >1000 opaque pixels, got {opaque}"

    def test_o_shape_opaque_pixel_count(self):
        """Rendered O-shape must contain hundreds of opaque pixels."""
        rgba = render_shape(o_shape())
        opaque = int((rgba[:, :, 3] == 255).sum())
        assert opaque > 100, f"Expected >100 opaque pixels, got {opaque}"

    def test_l_shape_top_left_transparent(self):
        """Top-left corner is outside the raster extent and must be transparent."""
        rgba = render_shape(l_shape())
        assert rgba[0, 0, 3] == 0, f"Top-left pixel alpha={rgba[0,0,3]}, expected 0"

    def test_o_shape_top_left_transparent(self):
        rgba = render_shape(o_shape())
        assert rgba[0, 0, 3] == 0, f"Top-left pixel alpha={rgba[0,0,3]}, expected 0"

    def test_l_shape_viridis_color_gradient(self):
        """Top rows of the L vertical arm should be purple; bottom rows yellow."""
        rgba = render_shape(l_shape())
        opaque = rgba[rgba[:, :, 3] == 255]
        assert opaque.size > 0, "No opaque pixels found"
        # viridis: low values → high blue channel, high values → high red+green
        # Split opaque pixels by their vertical position to check gradient direction
        rows, cols = np.where(rgba[:, :, 3] == 255)
        top_mask = rows < rows.mean()
        bot_mask = rows >= rows.mean()
        top_blue = rgba[rows[top_mask], cols[top_mask], 2].mean()
        bot_blue = rgba[rows[bot_mask], cols[bot_mask], 2].mean()
        assert top_blue > bot_blue, (
            f"Top pixels should be bluer (viridis low=purple): "
            f"top_blue={top_blue:.1f}, bot_blue={bot_blue:.1f}"
        )


# ── make_geotiff round-trip test ──────────────────────────────────────────────

class TestMakeGeotiff:
    def test_bytes_match_rust_fixture(self):
        """Python make_geotiff must produce identical bytes to the Rust fixture."""
        expected_path = os.path.join(FIXTURES_DIR, "l_shape.tiff")
        if not os.path.exists(expected_path):
            pytest.skip("l_shape.tiff fixture not found")
        with open(expected_path, "rb") as f:
            expected = f.read()
        produced = make_geotiff(l_shape(), 25, 25, 10.0, 0.225, 0.009, 0.0)
        assert produced == expected, (
            f"GeoTIFF byte mismatch: produced {len(produced)} bytes, "
            f"expected {len(expected)} bytes. "
            "First diff at byte "
            + str(next(i for i, (a, b) in enumerate(zip(produced, expected)) if a != b))
        )

    def test_o_shape_bytes_match_rust_fixture(self):
        expected_path = os.path.join(FIXTURES_DIR, "o_shape.tiff")
        if not os.path.exists(expected_path):
            pytest.skip("o_shape.tiff fixture not found")
        with open(expected_path, "rb") as f:
            expected = f.read()
        produced = make_geotiff(o_shape(), 25, 25, 10.0, 0.225, 0.009, 0.0)
        assert produced == expected


# ── Snapshot tests (mirror snapshot_l_shape_viridis / snapshot_o_shape_viridis)

def _assert_images_close(rendered: np.ndarray, expected: np.ndarray, label: str) -> None:
    """Two-part comparison that mirrors the Rust snapshot test intent.

    Alpha boundary check — rasterio's nearest-neighbour resampling and the Rust
    custom implementation can disagree by ±1 pixel at raster edges, so a small
    fraction of pixels may flip between transparent and opaque.  We allow ≤1%.

    Color accuracy check — the colormap LUT uses linear interpolation over 16
    key points; the Python and Rust implementations may round to adjacent steps,
    so we allow a max per-channel difference of 2 on pixels that are opaque in
    both renders.
    """
    assert rendered.shape == expected.shape, (
        f"{label}: shape mismatch {rendered.shape} vs {expected.shape}"
    )
    diff = np.abs(rendered.astype(int) - expected.astype(int))
    total = rendered.shape[0] * rendered.shape[1]

    # Alpha channel: only 1-pixel edge shifts are expected
    alpha_diff_pixels = int((diff[:, :, 3] > 0).sum())
    assert alpha_diff_pixels / total <= 0.01, (
        f"{label}: {alpha_diff_pixels}/{total} pixels have alpha differences "
        f"({alpha_diff_pixels/total:.2%}), tolerance is 1%. "
        "This suggests a large rendering misalignment, not just edge rounding."
    )

    # Color channels: compare only where both images are fully opaque
    both_opaque = (rendered[:, :, 3] == 255) & (expected[:, :, 3] == 255)
    if both_opaque.any():
        color_diff = diff[both_opaque, :3]
        max_color_diff = int(color_diff.max())
        assert max_color_diff <= 2, (
            f"{label}: max RGB diff on opaque pixels is {max_color_diff}, tolerance is 2. "
            "This suggests a colormap interpolation mismatch beyond rounding."
        )


class TestSnapshot:
    def test_l_shape_viridis(self):
        rendered = render_shape(l_shape())
        expected = _load_fixture_rgba("l_shape_viridis.png")
        _assert_images_close(rendered, expected, "l_shape_viridis")

    def test_o_shape_viridis(self):
        rendered = render_shape(o_shape())
        expected = _load_fixture_rgba("o_shape_viridis.png")
        _assert_images_close(rendered, expected, "o_shape_viridis")
