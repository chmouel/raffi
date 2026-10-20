"""Place a premultiplied RGBA window capture on the backdrop with simulated blur."""
import numpy as np
from PIL import Image

import backdrop

MARGIN = 36
BLUR_RADIUS = 20
SHADOW_RADIUS = 12
SHADOW_OFFSET = 8
DITHER = True


def _box(values, radius, axis):
    pad = [(0, 0)] * values.ndim
    pad[axis] = (radius + 1, radius)
    summed = np.cumsum(np.pad(values, pad, mode="edge"), axis=axis, dtype=np.float64)
    size = values.shape[axis]
    upper = np.take(summed, np.arange(2 * radius + 1, 2 * radius + 1 + size), axis=axis)
    lower = np.take(summed, np.arange(0, size), axis=axis)
    return ((upper - lower) / (2 * radius + 1)).astype(np.float32)


def _blur(values, sigma):
    """Gaussian approximation: three box blurs per axis."""
    radius = max(1, round((np.sqrt(4 * sigma * sigma + 1) - 1) / 2))
    for axis in (0, 1):
        for _ in range(3):
            values = _box(values, radius, axis)
    return values


def compose(window, theme, palette=None, crop=None, seed=1408):
    """`window` is an (h, w, 4) uint8 array, premultiplied RGBA. Returns an RGB Image."""
    win_h, win_w, _ = window.shape
    width, height = win_w + 2 * MARGIN, win_h + 2 * MARGIN
    rect = (MARGIN, MARGIN, win_w, win_h)
    name = backdrop.palette_for(theme, palette)
    sharp = backdrop.render(width, height, rect, name, seed=seed)
    tones = backdrop.PALETTES[name]

    # Shadow under the window, tinted towards the palette's deep colour.
    alpha = window[..., 3].astype(np.float32) / 255
    shape = np.zeros((height, width), dtype=np.float32)
    shape[MARGIN + SHADOW_OFFSET:MARGIN + SHADOW_OFFSET + win_h, MARGIN:MARGIN + win_w] = alpha
    shadow = _blur(shape, SHADOW_RADIUS) * (0.30 if tones.daylight else 0.50)
    shadow_rgb = backdrop._rgb(tones.shadow)
    canvas = sharp + (shadow_rgb - sharp) * shadow[..., None]

    # Hyprland-style blur: only the part of the backdrop behind the window is blurred.
    blurred = _blur(canvas, BLUR_RADIUS)
    region = (slice(MARGIN, MARGIN + win_h), slice(MARGIN, MARGIN + win_w))
    fg = window[..., :3].astype(np.float32) / 255
    behind = blurred[region]
    inside = fg + behind * (1 - alpha[..., None])
    mask = (alpha > 0)[..., None]
    canvas[region] = np.where(mask, inside, canvas[region])

    # Faint dither against banding, kept to the margin: inside the window it
    # would cost several times the file size for no visible gain.
    if DITHER:
        rng = np.random.default_rng(seed)
        noise = rng.random(canvas.shape[:2], dtype=np.float32) - 0.5
        noise[region] *= ~mask[..., 0]
        canvas += noise[..., None] / 255
    image = Image.fromarray((np.clip(canvas, 0, 1) * 255 + 0.5).astype(np.uint8), mode="RGB")
    if crop:
        x0, y0, x1, y1 = crop
        image = image.crop((x0 + MARGIN, y0 + MARGIN, x1 + MARGIN, y1 + MARGIN))
    return image


def save(image, path):
    image.save(path, optimize=True)
