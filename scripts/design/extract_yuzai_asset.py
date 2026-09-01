#!/usr/bin/env python3
"""Extract the canonical YeuX fish doodle from the project owner's paper scan.

The source scan contains old paper, a faint frame and text. This script crops
only the fish area, keeps connected dark-ink strokes, converts the paper to
alpha, and renders matched Paper/Nocturne monochrome assets. It deliberately
uses no generated geometry so the original asymmetry remains intact.
"""

from __future__ import annotations

import argparse
import sys
from collections import deque
from pathlib import Path

sys.dont_write_bytecode = True

from recolor_rgba_png import parse_hex_color, read_rgba_png, write_rgba_png


def luminance(red: int, green: int, blue: int) -> int:
    return (54 * red + 183 * green + 19 * blue) // 256


def connected_ink_mask(
    pixels: bytearray,
    width: int,
    height: int,
    roi: tuple[int, int, int, int],
    threshold: int,
    minimum_component: int,
) -> list[bool]:
    left, top, right, bottom = roi
    candidate = [False] * (width * height)
    for y in range(top, bottom):
        for x in range(left, right):
            pixel = (y * width + x) * 4
            if luminance(pixels[pixel], pixels[pixel + 1], pixels[pixel + 2]) < threshold:
                candidate[y * width + x] = True

    keep = [False] * (width * height)
    seen = [False] * (width * height)
    for y in range(top, bottom):
        for x in range(left, right):
            start = y * width + x
            if not candidate[start] or seen[start]:
                continue
            queue = deque([start])
            seen[start] = True
            component: list[int] = []
            while queue:
                current = queue.popleft()
                component.append(current)
                current_y, current_x = divmod(current, width)
                for delta_y in (-1, 0, 1):
                    for delta_x in (-1, 0, 1):
                        if delta_x == 0 and delta_y == 0:
                            continue
                        next_x, next_y = current_x + delta_x, current_y + delta_y
                        if not (left <= next_x < right and top <= next_y < bottom):
                            continue
                        next_index = next_y * width + next_x
                        if candidate[next_index] and not seen[next_index]:
                            seen[next_index] = True
                            queue.append(next_index)
            if len(component) >= minimum_component:
                for index in component:
                    keep[index] = True

    expanded = keep[:]
    for y in range(top, bottom):
        for x in range(left, right):
            index = y * width + x
            if not keep[index]:
                continue
            for delta_y in (-2, -1, 0, 1, 2):
                for delta_x in (-2, -1, 0, 1, 2):
                    next_x, next_y = x + delta_x, y + delta_y
                    if left <= next_x < right and top <= next_y < bottom:
                        expanded[next_y * width + next_x] = True
    return expanded


def mask_bounds(mask: list[bool], width: int, height: int) -> tuple[int, int, int, int]:
    xs: list[int] = []
    ys: list[int] = []
    for index, active in enumerate(mask):
        if active:
            y, x = divmod(index, width)
            xs.append(x)
            ys.append(y)
    if not xs:
        raise ValueError("no canonical ink found in the configured crop")
    return min(xs), min(ys), max(xs) + 1, max(ys) + 1


def source_alpha(pixels: bytearray, pixel_index: int, mask: list[bool]) -> int:
    source_pixel = pixel_index * 4
    if not mask[pixel_index]:
        return 0
    value = luminance(
        pixels[source_pixel], pixels[source_pixel + 1], pixels[source_pixel + 2]
    )
    return max(0, min(255, round((190 - value) * 255 / 165)))


def bilinear_alpha(
    pixels: bytearray,
    mask: list[bool],
    width: int,
    height: int,
    x: float,
    y: float,
) -> int:
    x0 = max(0, min(width - 1, int(x)))
    y0 = max(0, min(height - 1, int(y)))
    x1 = min(width - 1, x0 + 1)
    y1 = min(height - 1, y0 + 1)
    dx, dy = x - x0, y - y0
    a00 = source_alpha(pixels, y0 * width + x0, mask)
    a10 = source_alpha(pixels, y0 * width + x1, mask)
    a01 = source_alpha(pixels, y1 * width + x0, mask)
    a11 = source_alpha(pixels, y1 * width + x1, mask)
    top = a00 * (1 - dx) + a10 * dx
    bottom = a01 * (1 - dx) + a11 * dx
    return round(top * (1 - dy) + bottom * dy)


def render(
    source: bytearray,
    source_width: int,
    source_height: int,
    mask: list[bool],
    bounds: tuple[int, int, int, int],
    target: tuple[int, int, int],
    canvas: int,
    artwork_width: int,
    center_y: int,
) -> bytearray:
    left, top, right, bottom = bounds
    source_art_width = right - left
    source_art_height = bottom - top
    artwork_height = round(artwork_width * source_art_height / source_art_width)
    target_left = (canvas - artwork_width) // 2
    target_top = center_y - artwork_height // 2
    output = bytearray(canvas * canvas * 4)
    red, green, blue = target
    for target_y in range(max(0, target_top), min(canvas, target_top + artwork_height)):
        source_y = top + (target_y - target_top + 0.5) * source_art_height / artwork_height - 0.5
        for target_x in range(target_left, target_left + artwork_width):
            source_x = left + (target_x - target_left + 0.5) * source_art_width / artwork_width - 0.5
            alpha = bilinear_alpha(
                source, mask, source_width, source_height, source_x, source_y
            )
            if alpha == 0:
                continue
            output_pixel = (target_y * canvas + target_x) * 4
            output[output_pixel : output_pixel + 4] = bytes((red, green, blue, alpha))
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("paper_output", type=Path)
    parser.add_argument("nocturne_output", type=Path)
    parser.add_argument("--paper-color", type=parse_hex_color, default=parse_hex_color("#1B1815"))
    parser.add_argument("--nocturne-color", type=parse_hex_color, default=parse_hex_color("#D9D4C9"))
    parser.add_argument("--canvas", type=int, default=1200)
    parser.add_argument("--artwork-width", type=int, default=744)
    parser.add_argument("--center-y", type=int, default=515)
    args = parser.parse_args()

    width, height, pixels = read_rgba_png(args.input)
    roi = (
        round(width * 0.16),
        round(height * 0.20),
        round(width * 0.84),
        round(height * 0.66),
    )
    mask = connected_ink_mask(pixels, width, height, roi, threshold=150, minimum_component=5)
    bounds = mask_bounds(mask, width, height)
    for output, color in (
        (args.paper_output, args.paper_color),
        (args.nocturne_output, args.nocturne_color),
    ):
        rendered = render(
            pixels,
            width,
            height,
            mask,
            bounds,
            color,
            args.canvas,
            args.artwork_width,
            args.center_y,
        )
        write_rgba_png(output, args.canvas, args.canvas, rendered)
        print(f"wrote {output} ({args.canvas}x{args.canvas}, RGBA, source bounds={bounds})")


if __name__ == "__main__":
    main()
