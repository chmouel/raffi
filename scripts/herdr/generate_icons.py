#!/usr/bin/env python3
import argparse
from pathlib import Path
import xml.etree.ElementTree as ET


ICON_DIR = Path(__file__).resolve().parent / "icons"
PRESETS = ("color", "mono-dark", "mono-light")
COLORS = {
    "workspace": ("#A99AF2", "#302A45"),
    "session": ("#83B7E8", "#25384B"),
    "shell": ("#B2BFCD", "#2B3541"),
    "agent-claude": ("#F0B294", "#49342F"),
    "agent-copilot": ("#C6BAF5", "#37304B"),
    "agent-pi": ("#EACB84", "#433A28"),
    "agent-gemini": ("#9EBFF5", "#2C354D"),
    "agent-codex": ("#A6D8CA", "#2A403D"),
    "status-working": ("#A6D8CA", "#2A403D"),
    "status-waiting": ("#F0B294", "#49342F"),
    "status-idle": ("#9EBFF5", "#2C354D"),
    "status-done": ("#EACB84", "#433A28"),
    "status-unknown": ("#B2BFCD", "#2B3541"),
}
DEFAULT_COLORS = ("#B9D4E7", "#2D3B49")
SVG = "http://www.w3.org/2000/svg"
ET.register_namespace("", SVG)


def render_icon(source, preset):
    mark = ET.fromstring(source.read_text())
    if preset == "color":
        foreground, background = COLORS.get(source.stem, DEFAULT_COLORS)
    else:
        foreground = "#EEEAF4" if preset == "mono-dark" else "#343342"
        background = None
    for element in mark.iter():
        for name, value in list(element.attrib.items()):
            if value == "currentColor":
                element.set(name, foreground)
    root = ET.Element(
        f"{{{SVG}}}svg",
        {"viewBox": "0 0 32 32", "width": "32", "height": "32"},
    )
    if background:
        ET.SubElement(
            root,
            f"{{{SVG}}}rect",
            {"width": "32", "height": "32", "rx": "8", "fill": background},
        )
    mark.set("x", "6")
    mark.set("y", "6")
    mark.set("width", "20")
    mark.set("height", "20")
    root.append(mark)
    return ET.tostring(root, encoding="unicode") + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    stale = []
    for source in sorted((ICON_DIR / "source").glob("*.svg")):
        for preset in PRESETS:
            target = ICON_DIR / preset / source.name
            content = render_icon(source, preset)
            if args.check:
                if not target.is_file() or target.read_text() != content:
                    stale.append(str(target))
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content)
    if stale:
        parser.exit(
            1,
            "Run uv run --no-project python3 scripts/herdr/generate_icons.py to refresh:\n"
            + "\n".join(stale)
            + "\n",
        )


if __name__ == "__main__":
    main()
