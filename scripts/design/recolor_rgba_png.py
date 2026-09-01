#!/usr/bin/env python3
"""Deterministically recolor a non-interlaced 8-bit RGB/RGBA PNG.

This tiny, dependency-free helper exists for project-owned monochrome artwork.
It preserves the source alpha and turns source luminance into additional ink
coverage, so dry edges survive when graphite artwork is recolored for a dark
theme. It intentionally rejects other PNG formats instead of guessing.
"""

from __future__ import annotations

import argparse
import binascii
import struct
import zlib
from pathlib import Path


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
OUTPUT_BYTES_PER_PIXEL = 4


def paeth(a: int, b: int, c: int) -> int:
    prediction = a + b - c
    distance_a = abs(prediction - a)
    distance_b = abs(prediction - b)
    distance_c = abs(prediction - c)
    if distance_a <= distance_b and distance_a <= distance_c:
        return a
    if distance_b <= distance_c:
        return b
    return c


def read_rgba_png(path: Path) -> tuple[int, int, bytearray]:
    data = path.read_bytes()
    if not data.startswith(PNG_SIGNATURE):
        raise ValueError(f"not a PNG: {path}")

    cursor = len(PNG_SIGNATURE)
    width = height = 0
    source_bytes_per_pixel = 0
    compressed = bytearray()
    while cursor < len(data):
        length = struct.unpack(">I", data[cursor : cursor + 4])[0]
        kind = data[cursor + 4 : cursor + 8]
        payload = data[cursor + 8 : cursor + 8 + length]
        cursor += 12 + length
        if kind == b"IHDR":
            width, height, depth, color_type, compression, filtering, interlace = struct.unpack(
                ">IIBBBBB", payload
            )
            if depth != 8 or color_type not in (2, 6) or (compression, filtering, interlace) != (0, 0, 0):
                raise ValueError(
                    "expected non-interlaced 8-bit RGB/RGBA PNG "
                    f"but got depth={depth}, color_type={color_type}, interlace={interlace}"
                )
            source_bytes_per_pixel = 3 if color_type == 2 else 4
        elif kind == b"IDAT":
            compressed.extend(payload)
        elif kind == b"IEND":
            break

    raw = zlib.decompress(bytes(compressed))
    stride = width * source_bytes_per_pixel
    expected = height * (stride + 1)
    if len(raw) != expected:
        raise ValueError(f"unexpected decompressed size: {len(raw)} != {expected}")

    source_pixels = bytearray(width * height * source_bytes_per_pixel)
    previous = bytearray(stride)
    offset = 0
    for row_index in range(height):
        filter_type = raw[offset]
        offset += 1
        encoded = raw[offset : offset + stride]
        offset += stride
        decoded = bytearray(stride)
        for index, value in enumerate(encoded):
            left = decoded[index - source_bytes_per_pixel] if index >= source_bytes_per_pixel else 0
            above = previous[index]
            upper_left = previous[index - source_bytes_per_pixel] if index >= source_bytes_per_pixel else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = left
            elif filter_type == 2:
                predictor = above
            elif filter_type == 3:
                predictor = (left + above) // 2
            elif filter_type == 4:
                predictor = paeth(left, above, upper_left)
            else:
                raise ValueError(f"unsupported PNG filter {filter_type}")
            decoded[index] = (value + predictor) & 0xFF
        start = row_index * stride
        source_pixels[start : start + stride] = decoded
        previous = decoded

    if source_bytes_per_pixel == OUTPUT_BYTES_PER_PIXEL:
        return width, height, source_pixels
    pixels = bytearray(width * height * OUTPUT_BYTES_PER_PIXEL)
    for source_index in range(0, len(source_pixels), source_bytes_per_pixel):
        output_index = source_index // source_bytes_per_pixel * OUTPUT_BYTES_PER_PIXEL
        pixels[output_index : output_index + 4] = source_pixels[source_index : source_index + 3] + b"\xff"
    return width, height, pixels


def chunk(kind: bytes, payload: bytes) -> bytes:
    return (
        struct.pack(">I", len(payload))
        + kind
        + payload
        + struct.pack(">I", binascii.crc32(kind + payload) & 0xFFFFFFFF)
    )


def write_rgba_png(path: Path, width: int, height: int, pixels: bytearray) -> None:
    stride = width * OUTPUT_BYTES_PER_PIXEL
    scanlines = bytearray()
    for row_index in range(height):
        scanlines.append(0)
        start = row_index * stride
        scanlines.extend(pixels[start : start + stride])
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    encoded = PNG_SIGNATURE + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(scanlines), 9)) + chunk(b"IEND", b"")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(encoded)


def parse_hex_color(value: str) -> tuple[int, int, int]:
    normalized = value.removeprefix("#")
    if len(normalized) != 6:
        raise argparse.ArgumentTypeError("color must be #RRGGBB")
    try:
        return tuple(int(normalized[index : index + 2], 16) for index in (0, 2, 4))  # type: ignore[return-value]
    except ValueError as error:
        raise argparse.ArgumentTypeError("color must be #RRGGBB") from error


def recolor(pixels: bytearray, target: tuple[int, int, int]) -> None:
    target_r, target_g, target_b = target
    for index in range(0, len(pixels), OUTPUT_BYTES_PER_PIXEL):
        red, green, blue, alpha = pixels[index : index + 4]
        if alpha == 0:
            pixels[index : index + 4] = bytes((0, 0, 0, 0))
            continue
        luminance = (54 * red + 183 * green + 19 * blue) // 256
        ink = 255 - luminance
        output_alpha = (alpha * ink + 127) // 255
        pixels[index : index + 4] = bytes((target_r, target_g, target_b, output_alpha))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--color", type=parse_hex_color, required=True)
    args = parser.parse_args()

    width, height, pixels = read_rgba_png(args.input)
    recolor(pixels, args.color)
    write_rgba_png(args.output, width, height, pixels)
    print(f"wrote {args.output} ({width}x{height}, RGBA)")


if __name__ == "__main__":
    main()
