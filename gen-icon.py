#!/usr/bin/env python3
"""Generate FnKey app icon using Pillow."""
import math
import os
import shutil
import subprocess
from PIL import Image, ImageDraw, ImageFont


def draw_icon(size):
    """Draw the FnKey icon at given pixel size."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    s = size
    pad = int(s * 0.08)
    corner_r = int(s * 0.22)

    # === Background: dark rounded square ===
    draw.rounded_rectangle(
        [pad, pad, s - pad, s - pad],
        radius=corner_r,
        fill=(28, 28, 32, 255),
    )

    # Subtle border
    inset = pad + max(1, int(s * 0.006))
    draw.rounded_rectangle(
        [inset, inset, s - inset, s - inset],
        radius=corner_r - max(1, int(s * 0.006)),
        outline=(60, 60, 72, 130),
        width=max(1, int(s * 0.004)),
    )

    cx = s / 2
    cy = s / 2

    # === Sound wave arcs ===
    for radius_frac, alpha in [(0.30, 35), (0.23, 55), (0.16, 80)]:
        r = s * radius_frac
        arc_w = max(1, int(s * 0.013))
        arc_color = (100, 190, 255, alpha)
        # Right arcs
        bbox = [cx + s*0.02 - r, cy + s*0.04 - r, cx + s*0.02 + r, cy + s*0.04 + r]
        draw.arc(bbox, start=-50, end=50, fill=arc_color, width=arc_w)
        # Left arcs
        bbox = [cx - s*0.02 - r, cy + s*0.04 - r, cx - s*0.02 + r, cy + s*0.04 + r]
        draw.arc(bbox, start=130, end=230, fill=arc_color, width=arc_w)

    # === Microphone ===
    mic_w = s * 0.14
    mic_h = s * 0.24
    mic_x = cx - mic_w / 2
    mic_y = cy - mic_h * 0.15

    mic_color = (100, 195, 255, 240)

    # Mic capsule (pill shape)
    mic_r = mic_w / 2
    draw.rounded_rectangle(
        [mic_x, mic_y, mic_x + mic_w, mic_y + mic_h],
        radius=int(mic_r),
        fill=mic_color,
    )

    # Grille lines on mic
    grille_color = (35, 100, 160, 100)
    num_lines = 4
    grille_top = mic_y + mic_h * 0.28
    grille_bot = mic_y + mic_h * 0.82
    line_w = max(1, int(s * 0.005))
    for i in range(num_lines):
        ly = grille_top + i * (grille_bot - grille_top) / (num_lines - 1)
        lx1 = mic_x + mic_w * 0.2
        lx2 = mic_x + mic_w * 0.8
        draw.line([(lx1, ly), (lx2, ly)], fill=grille_color, width=line_w)

    # === Mic stand ===
    stand_color = (100, 195, 255, 200)
    stand_w = max(1, int(s * 0.016))

    # U-cradle arc
    cradle_r = mic_w * 0.85
    cradle_cy = mic_y + mic_h * 0.08
    bbox = [cx - cradle_r, cradle_cy - cradle_r, cx + cradle_r, cradle_cy + cradle_r]
    draw.arc(bbox, start=0, end=180, fill=stand_color, width=stand_w)

    # Vertical stem
    stem_top = cradle_cy + cradle_r
    stem_bottom = stem_top + s * 0.07
    draw.line([(cx, stem_top), (cx, stem_bottom)], fill=stand_color, width=stand_w)

    # Base
    base_w = s * 0.12
    draw.line(
        [(cx - base_w/2, stem_bottom), (cx + base_w/2, stem_bottom)],
        fill=stand_color,
        width=stand_w,
    )

    # === "fn" text at bottom ===
    font_size = int(s * 0.16)
    try:
        font = ImageFont.truetype("/System/Library/Fonts/HelveticaNeue.ttc", font_size, index=8)  # Bold
    except (OSError, IndexError):
        try:
            font = ImageFont.truetype("/System/Library/Fonts/Helvetica.ttc", font_size)
        except OSError:
            font = ImageFont.load_default()

    text = "fn"
    text_bbox = draw.textbbox((0, 0), text, font=font)
    tw = text_bbox[2] - text_bbox[0]
    th = text_bbox[3] - text_bbox[1]

    text_x = cx - tw / 2 - text_bbox[0]
    text_y = pad + s * 0.06

    draw.text((text_x, text_y), text, fill=(255, 255, 255, 230), font=font)

    return img


def main():
    script_dir = os.path.dirname(os.path.abspath(__file__))
    iconset_dir = os.path.join(script_dir, "AppIcon.iconset")
    os.makedirs(iconset_dir, exist_ok=True)

    sizes = [
        (16, 1), (16, 2),
        (32, 1), (32, 2),
        (128, 1), (128, 2),
        (256, 1), (256, 2),
        (512, 1), (512, 2),
    ]

    for base_size, scale in sizes:
        px = base_size * scale
        if scale == 1:
            name = f"icon_{base_size}x{base_size}.png"
        else:
            name = f"icon_{base_size}x{base_size}@{scale}x.png"

        path = os.path.join(iconset_dir, name)
        img = draw_icon(px)
        img.save(path, "PNG")
        print(f"  {name} ({px}x{px})")

    # Convert to .icns
    icns_path = os.path.join(script_dir, "AppIcon.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset_dir, "-o", icns_path], check=True)
    print(f"\nCreated {icns_path}")

    shutil.rmtree(iconset_dir)


if __name__ == "__main__":
    main()
