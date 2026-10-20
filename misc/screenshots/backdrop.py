""""Night swell" backdrop: a moonlit sea seen from above, drawn from the logo.

Flowing contour lines glow where the swell crests, with a few four-point
sparkles like the logo's stars. One bright swell runs diagonally from lower
left to upper right, the spout's direction, and passes behind the window so
the simulated blur shows at a glance.
"""
from dataclasses import dataclass

import numpy as np
from PIL import Image

HERO_ANGLE = 24


def _rgb(value):
    value = value.lstrip("#")
    return np.array([int(value[i:i + 2], 16) for i in (0, 2, 4)], dtype=np.float32) / 255


@dataclass(frozen=True)
class Palette:
    top: str
    bottom: str
    line: str
    crest: str
    hero_from: str
    hero_to: str
    sparkle: str
    shadow: str
    daylight: bool = False


PALETTES = {
    # The logo: abyss #121C45, deep #23357F, lavender #B7A4F4, sky #8CC6FF, foam #E9F1FF.
    "night": Palette("#0d1433", "#1c2b6b", "#8cc6ff", "#e9f1ff", "#b7a4f4", "#8cc6ff", "#e9f1ff", "#050a22"),
    "catppuccin": Palette("#11111b", "#2a2f5e", "#89b4fa", "#cdd6f4", "#cba6f7", "#89b4fa", "#f5e0dc", "#07070f"),
    "nord": Palette("#1a202b", "#2e3f5c", "#88c0d0", "#eceff4", "#81a1c1", "#8fbcbb", "#eceff4", "#0b0f16"),
    "gruvbox": Palette("#1a1712", "#3d3220", "#d79921", "#fabd2f", "#fe8019", "#fabd2f", "#fbf1c7", "#0c0a07"),
    "tokyonight": Palette("#12131f", "#2b2352", "#7aa2f7", "#c0caf5", "#bb9af7", "#7dcfff", "#c0caf5", "#07070f"),
    "daylight": Palette("#f4f8ff", "#cfe2fb", "#3a57b8", "#23357f", "#9d86f0", "#4c9ff0", "#7b6cd9", "#23357f", True),
    "daylight-warm": Palette("#fdf8ea", "#d7e8f3", "#268bd2", "#1d5f8f", "#9d86f0", "#2aa198", "#6c71c4", "#3b4a63", True),
}

THEME_PALETTES = {
    "dark": "night",
    "catppuccin": "catppuccin",
    "catppuccin-mocha": "catppuccin",
    "nord": "nord",
    "gruvbox": "gruvbox",
    "tokyonight": "tokyonight",
    "tokyo-night": "tokyonight",
    "light": "daylight",
    "catppuccin-latte": "daylight",
}


def _noise(rng, width, height, cells):
    """Smooth value noise in [-1, 1], `cells` lattice cells across the height."""
    grid_h = max(2, cells)
    grid_w = max(2, round(cells * width / height))
    grid = rng.random((grid_h + 3, grid_w + 3)).astype(np.float32)
    image = Image.fromarray(grid, mode="F").resize((width, height), Image.BICUBIC, box=(1, 1, grid_w + 1, grid_h + 1))
    values = np.asarray(image)
    return (values - values.mean()) / (np.abs(values - values.mean()).max() + 1e-6)


def _lines(field, width_px, glow_px):
    """Anti-aliased iso-lines at integer values of `field`, plus a soft halo."""
    gy, gx = np.gradient(field)
    slope = np.hypot(gx, gy) + 1e-6
    distance = np.abs(field - np.round(field)) / slope
    core = np.clip(width_px - distance + 0.5, 0, 1)
    halo = np.exp(-(distance / glow_px) ** 2)
    return core, halo


def _mix(base, color, alpha):
    return base + (color - base) * alpha[..., None]


def _sparkles(canvas, rng, palette, count):
    height, width, _ = canvas.shape
    color = _rgb(palette.sparkle)
    for _ in range(count):
        cx, cy = rng.uniform(0, width), rng.uniform(0, height)
        arm = rng.uniform(4, 11)
        strength = rng.uniform(0.45, 0.95)
        radius = int(arm * 3) + 2
        x0, x1 = max(0, int(cx) - radius), min(width, int(cx) + radius + 1)
        y0, y1 = max(0, int(cy) - radius), min(height, int(cy) + radius + 1)
        if x0 >= x1 or y0 >= y1:
            continue
        ys, xs = np.mgrid[y0:y1, x0:x1].astype(np.float32)
        dx, dy = np.abs(xs - cx), np.abs(ys - cy)
        star = (np.exp(-dx / 0.7) * np.exp(-(dy / arm) ** 2) + np.exp(-dy / 0.7) * np.exp(-(dx / arm) ** 2)) * 0.8
        star += np.exp(-(dx ** 2 + dy ** 2) / 2.5)
        star += 0.18 * np.exp(-(dx ** 2 + dy ** 2) / (arm * 1.2) ** 2)
        patch = canvas[y0:y1, x0:x1]
        canvas[y0:y1, x0:x1] = _mix(patch, color, np.clip(star * strength, 0, 1))


def render(width, height, window, palette_name, seed=1408):
    """Return the backdrop as float32 RGB in [0, 1].

    `window` is (x, y, w, h); the hero swell is placed through its centre.
    """
    palette = PALETTES[palette_name]
    rng = np.random.default_rng(seed)
    ys, xs = np.mgrid[0:height, 0:width].astype(np.float32)
    unit = float(height)

    # Base: vertical gradient with a broad, noisy wash of colour.
    t = ys / height
    wash = _noise(rng, width, height, 2)
    base = _rgb(palette.top) * (1 - t[..., None]) + _rgb(palette.bottom) * t[..., None]
    base = _mix(base, _rgb(palette.bottom), np.clip(0.25 + 0.35 * wash, 0, 1) * (0.5 if palette.daylight else 0.6))

    # Swells run lower left to upper right at HERO_ANGLE, shallower than the
    # image diagonal so the hero crosses both side margins, not just corners.
    angle = np.radians(HERO_ANGLE)
    normal = np.array([np.sin(angle), np.cos(angle)], dtype=np.float32)
    across = (xs * normal[0] + ys * normal[1]) / unit
    wx, wy, ww, wh = window
    centre = ((wx + ww / 2) * normal[0] + (wy + wh / 2) * normal[1]) / unit
    along = (xs * normal[1] - ys * normal[0]) / unit

    warp = _noise(rng, width, height, 3)
    ripple = _noise(rng, width, height, 9)
    crest_noise = _noise(rng, width, height, 4)

    swell = across * 9 + 0.8 * warp + 0.12 * ripple + 0.35 * np.sin(along * 3.1 + warp * 1.5)
    crest = np.clip((np.sin(swell * np.pi * 2 / 3 + crest_noise * 2.5) + 0.2) / 1.2, 0, 1) ** 2

    # Second, finer family of swells underneath, fainter.
    under = across * 23 + 1.9 * warp - 0.3 * ripple + 0.3 * np.sin(along * 5.3)
    under_core, _ = _lines(under, 0.6, 2)

    core, halo = _lines(swell, 0.9, 7)
    line = _rgb(palette.line)
    crest_color = _rgb(palette.crest)
    soft = 0.10 if palette.daylight else 0.07
    canvas = _mix(base, line, under_core * (0.08 if palette.daylight else 0.10))
    canvas = _mix(canvas, line, halo * (soft + 0.18 * crest))
    canvas = _mix(canvas, line, core * (0.22 + 0.25 * crest))
    canvas = _mix(canvas, crest_color, core * crest * 0.55)

    # The hero swell, bending gently through the window centre.
    offset = across - centre - 0.10 * warp - 0.04 * np.sin(along * 2.2)
    span = np.clip((along - along.min()) / (along.max() - along.min()), 0, 1)
    hero_color = _rgb(palette.hero_from) * (1 - span[..., None]) + _rgb(palette.hero_to) * span[..., None]
    broad = np.exp(-(offset / 0.28) ** 2)
    band = np.exp(-(offset / 0.10) ** 2)
    hero_lines = across * 30 + 0.8 * ripple
    hero_core, _ = _lines(hero_lines, 0.8, 3)
    spine = np.exp(-((offset * unit) / 2.8) ** 2)
    canvas = _mix(canvas, hero_color, broad * (0.40 if palette.daylight else 0.45))
    canvas = _mix(canvas, hero_color, band * (0.35 + 0.45 * hero_core))
    glow = np.exp(-((offset * unit) / 18) ** 2)
    canvas = _mix(canvas, hero_color, glow * (0.50 if palette.daylight else 0.65))
    canvas = _mix(canvas, crest_color if not palette.daylight else hero_color, spine * 0.9)

    # Vignette keeps the eye on the window.
    cx, cy = wx + ww / 2, wy + wh / 2
    radial = np.hypot((xs - cx) / width, (ys - cy) / height)
    shade = np.clip(radial * 0.55, 0, 0.35)
    canvas = _mix(canvas, _rgb(palette.top) * (1.03 if palette.daylight else 0.6), shade)

    _sparkles(canvas, rng, palette, max(6, int(width * height / 45000)))
    return np.clip(canvas, 0, 1).astype(np.float32)


def palette_for(theme, override=None):
    return override or THEME_PALETTES.get(theme or "dark", "night")
