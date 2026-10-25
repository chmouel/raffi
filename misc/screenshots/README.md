# Screenshots

This tool regenerates the native UI screenshots in
`website/src/assets/screenshots/` from fixtures, so you can re-shoot them after
a visible UI change without touching your own desktop. `fuzzel-mode.png` is
not covered. It shows Fuzzel's own window and needs a real Wayland session.

```sh
make screenshots-list                       # catalogue and prerequisite check
make screenshots                            # every shot
make screenshots SHOTS="main-search calculator"
```

`make screenshots` builds the release binary first. To skip the build, call
the script directly with `uv run misc/screenshots/shoot.py [names...]`. It
also takes `--out DIR` to write somewhere else, `--binary PATH` to use another
binary, and `--keep` to keep the temporary home and logs.

## Prerequisites

You need:

- [uv](https://docs.astral.sh/uv/). It installs numpy, Pillow and
  python-xlib from the inline metadata in `shoot.py`.
- Xvfb.
- A Vulkan driver. Mesa's lavapipe is enough. The GPU renderer is what gives
  the window an alpha channel.
- The Noto Sans and JetBrains Mono fonts. Icons come from the system icon
  themes. The checked-in shots were taken with Papirus installed.

`--list` checks these and says what is missing.

## How it works

Each run starts with a fresh `/tmp/raffi-screenshots/home`. It writes the demo
config, files and caches from `fixtures/demo.py` into it. Commands that the
addons call, such as `pactl`, `niri`, `gh` and `rbw`, are symlinks to
`fixtures/fake_cli.py`, which prints canned demo data. The browser tabs shots
talk to a fake socket from `fixtures/browser_host.py`. Raffi runs with a
scrubbed environment and has no access to your Wayland session, D-Bus,
tokens or password store.

Every shot runs raffi on a private Xvfb display. The script types the keys,
then grabs the window together with its alpha. `composite.py` places it on
the generated "night swell" backdrop from `backdrop.py`, blurs the backdrop
behind the window to mimic compositor blur, and adds a margin and shadow.
The backdrop is seeded, so every shot shows the same desktop. Each theme
gets its own tint, and the light themes get a daylight version.

## Adding or changing a shot

Edit `catalogue.py`. A `Shot` has a name (the PNG file name), a query, keys
to press after the window maps (`ctrl+j`, `Down`, `type:some text`), a theme,
the window size and an optional `tweak` that edits the config. `classic=True`
uses the smaller window and the extra demo sources from the older shots.
`needs` starts helpers such as the browser socket. `new_user=True` removes
the fixture config and usage caches, applying demo settings through CLI
overrides so the first-run flow can be captured. Demo data shared by
several shots lives in `fixtures/demo.py`.

Run the shot on its own, look at the PNG, then reference it from the docs.

## Known differences between runs

A few things depend on the outside world. The currency converter uses live
rates from api.frankfurter.dev, and the `{date}` snippet placeholders and
world clock show the real current date. The kill-process shot shows the PID
of a throwaway `sleep` process. Re-shooting those changes their pixels even
when the UI did not change.
