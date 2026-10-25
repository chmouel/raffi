#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy", "pillow", "python-xlib"]
# ///
"""Regenerate Raffi's native UI screenshots from fixtures.

Runs the release binary under a private Xvfb with a throwaway HOME
(/tmp/raffi-screenshots/home), fake CLIs on PATH and no access to the real
session, captures each window with its alpha channel, and composites it on
the generated backdrop.

    uv run misc/screenshots/shoot.py                  # every shot
    uv run misc/screenshots/shoot.py main calculator  # some shots
    uv run misc/screenshots/shoot.py --list           # catalogue and prerequisites
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path[:0] = [str(HERE), str(HERE / "fixtures")]

import catalogue  # noqa: E402
import composite  # noqa: E402
import demo  # noqa: E402

OUT = REPO / "website/src/assets/screenshots"
BINARY = REPO / "target/release/raffi"
FONTS = ["Noto Sans", "JetBrains Mono"]
ROOT = Path("/tmp/raffi-screenshots")
SCREEN = "1400x900x24"
MAP_DELAY = 2.5
KEY_DELAY = 0.15
TYPED = {" ": "space", "/": "slash", ".": "period", "-": "minus", "~": "asciitilde", ":": "colon", "+": "plus"}


def check_prerequisites(binary):
    problems = []
    for tool in ["Xvfb", "fc-match", "python3"]:
        if not shutil.which(tool):
            problems.append(f"{tool} not found on PATH")
    if not binary.exists():
        problems.append(f"{binary} missing: run `cargo build --release` or `make build`")
    if shutil.which("fc-match"):
        for font in FONTS:
            family = subprocess.run(["fc-match", "-f", "%{family}", font], capture_output=True, text=True, check=False).stdout
            if font not in family:
                problems.append(f"font {font!r} not installed (fc-match gives {family!r}); text will render differently")
    if not any(Path(path).glob("*.json") for path in ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"]):
        problems.append("no Vulkan ICD found (install mesa's lavapipe or a GPU driver); alpha capture needs the GPU renderer")
    return problems


class Session:
    """Xvfb, the demo HOME and the scrubbed environment Raffi runs in."""

    def __init__(self, binary, root):
        self.binary = binary
        self.root = root
        self.home = root / "home"
        self.runtime = self.home / "runtime"
        self.base = demo.setup(self.home, REPO)
        demo.link_fake_commands(root / "bin")
        self.xvfb = None
        self.display = None
        self.helpers = []

    def start(self):
        read, write = os.pipe()
        self.xvfb = subprocess.Popen(
            ["Xvfb", "-displayfd", str(write), "-screen", "0", SCREEN, "+extension", "COMPOSITE", "-nolisten", "tcp"],
            pass_fds=[write], stderr=subprocess.DEVNULL,
        )
        os.close(write)
        with os.fdopen(read) as pipe:
            number = pipe.readline().strip()
        if not number:
            raise RuntimeError("Xvfb did not start")
        self.display = f":{number}"
        self.env = {
            "PATH": f"{self.root / 'bin'}:/usr/bin:/bin",
            "DISPLAY": self.display,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_CACHE_HOME": str(self.home / ".cache"),
            "XDG_DATA_HOME": str(self.home / ".local/share"),
            "XDG_RUNTIME_DIR": str(self.runtime),
            "XDG_DATA_DIRS": "/usr/local/share:/usr/share",
            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={self.root / 'no-session-bus'}",
            # The window switcher looks for a compositor socket; the fake niri answers.
            "NIRI_SOCKET": str(self.runtime / "demo-niri"),
            "PASSWORD_STORE_DIR": str(self.home / ".password-store"),
            "TZ": "Europe/Paris",
            "LANG": "C.UTF-8",
            "GIT_CONFIG_GLOBAL": "/dev/null",
        }

    def helper(self, *args, **kwargs):
        process = subprocess.Popen(*args, **kwargs)
        self.helpers.append(process)
        return process

    def browser_socket(self):
        directory = self.runtime / "raffi/browsers"
        process = self.helper([sys.executable, str(HERE / "fixtures/browser_host.py"), str(directory)],
                              stdout=subprocess.PIPE, text=True)
        if process.stdout.readline().strip() != "ready":
            raise RuntimeError("browser tabs fixture did not start")
        return process

    def stop(self):
        for process in [*self.helpers, self.xvfb]:
            if process and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        self.helpers = []

    def config(self, shot):
        config = json.loads(json.dumps(self.base))
        general = config["general"]
        if shot.classic:
            catalogue.classic(config)
        else:
            general.update({"window_width": shot.width, "window_height": shot.height})
        if shot.theme:
            general["theme"] = shot.theme
        if shot.tweak:
            shot.tweak(config)
        return config

    def capture(self, shot):
        from Xlib import X, display

        context = {}
        worker = None
        if "worker" in shot.needs:
            worker = subprocess.Popen(["raffi-demo-worker", "300"], executable="/usr/bin/sleep")
            self.helpers.append(worker)
            context["worker"] = worker.pid
        browser = self.browser_socket() if "browser" in shot.needs else None
        if shot.name == "launcher-script-preview":
            (self.home / ".cache/raffi/mru.cache").write_text(f"Check Disk Usage|19|{catalogue.CHECK_DISK_LAST_USED}\n")

        config = self.config(shot)
        path = self.root / "config.json"
        path.write_text(json.dumps(config, ensure_ascii=False))
        log = (self.root / "logs" / f"{shot.name}.log").open("w")
        command = [str(self.binary), "-u", "native", "-c", str(path), "-i", shot.query.format(**context),
                   "--app-name", "raffi-screenshots"]
        env = {key: value for key, value in self.env.items() if key not in shot.unset_env}
        process = subprocess.Popen(command, env=env, stdout=log, stderr=log)
        conn = display.Display(self.display)
        try:
            window = self._wait_for_window(conn, process, shot)
            window.set_input_focus(X.RevertToParent, X.CurrentTime)
            conn.sync()
            time.sleep(MAP_DELAY)
            for key in shot.keys:
                self._press(conn, key)
            time.sleep(shot.settle)
            pixels = self._grab(window, shot)
        finally:
            conn.close()
            log.close()
            for helper in [process, worker, browser]:
                if helper and helper.poll() is None:
                    helper.terminate()
                    helper.wait(timeout=5)
            (self.home / ".cache/raffi/mru.cache").unlink(missing_ok=True)
        theme = config["general"]["theme"]
        return composite.compose(pixels, theme, palette=shot.palette, crop=shot.crop)

    @staticmethod
    def _wait_for_window(conn, process, shot):
        from Xlib import X

        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"{shot.name}: raffi exited with {process.returncode}")
            for child in conn.screen().root.query_tree().children:
                if child.get_attributes().map_state == X.IsViewable:
                    return child
            time.sleep(0.1)
        raise RuntimeError(f"{shot.name}: no window after 20s")

    @staticmethod
    def _press(conn, key):
        from Xlib import XK, X
        from Xlib.ext import xtest

        if key.startswith("type:"):
            for char in key[5:]:
                Session._press(conn, ("shift+" + char.lower()) if char.isupper() else TYPED.get(char, char))
            return
        *modifiers, name = key.split("+")
        names = {"ctrl": "Control_L", "alt": "Alt_L", "shift": "Shift_L"}
        codes = list(conn.keysym_to_keycodes(XK.string_to_keysym(name)))
        if not codes:
            raise ValueError(f"no keycode for {key!r}")
        code, index = min(codes, key=lambda c: c[1])
        # keysyms in the shifted column (":", "+", "~" on a US map) need Shift held
        if index % 2 and "shift" not in modifiers:
            modifiers.append("shift")
        held = [conn.keysym_to_keycode(XK.string_to_keysym(names[m])) for m in modifiers]
        for modifier in held:
            xtest.fake_input(conn, X.KeyPress, modifier)
        xtest.fake_input(conn, X.KeyPress, code)
        xtest.fake_input(conn, X.KeyRelease, code)
        for modifier in reversed(held):
            xtest.fake_input(conn, X.KeyRelease, modifier)
        conn.sync()
        time.sleep(KEY_DELAY)

    @staticmethod
    def _grab(window, shot):
        from Xlib import X

        geometry = window.get_geometry()
        if geometry.depth != 32:
            raise RuntimeError(f"{shot.name}: window depth is {geometry.depth}, not 32; "
                               "the GPU renderer did not get an ARGB visual (check Vulkan)")
        raw = window.get_image(0, 0, geometry.width, geometry.height, X.ZPixmap, 0xFFFFFFFF)
        pixels = np.frombuffer(raw.data, dtype=np.uint8).reshape(geometry.height, geometry.width, 4)
        return pixels[..., [2, 1, 0, 3]].copy()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("shots", nargs="*", help="shot names (default: all)")
    parser.add_argument("--list", action="store_true", help="print the catalogue and check prerequisites")
    parser.add_argument("--out", type=Path, default=OUT, help=f"output directory (default: {OUT.relative_to(REPO)})")
    parser.add_argument("--binary", type=Path, default=BINARY, help="raffi binary (default: target/release/raffi)")
    parser.add_argument("--keep", action="store_true", help="keep the temporary HOME and logs")
    args = parser.parse_args()

    problems = check_prerequisites(args.binary)
    if args.list:
        for shot in catalogue.SHOTS:
            size = "960x560" if shot.classic else f"{shot.width}x{shot.height}"
            print(f"{shot.name:28} {size:9} {shot.query!r}{'  ' + ' '.join(shot.keys) if shot.keys else ''}")
        print(f"\n{len(catalogue.SHOTS)} shots")
        for problem in problems:
            print(f"warning: {problem}", file=sys.stderr)
        return 1 if problems else 0

    unknown = [name for name in args.shots if name not in catalogue.BY_NAME]
    if unknown:
        parser.error(f"unknown shot(s): {', '.join(unknown)} (see --list)")
    fatal = [p for p in problems if not p.startswith("font ")]
    for problem in problems:
        print(f"{'error' if problem in fatal else 'warning'}: {problem}", file=sys.stderr)
    if fatal:
        return 1

    shots = [catalogue.BY_NAME[name] for name in args.shots] or catalogue.SHOTS
    args.out.mkdir(parents=True, exist_ok=True)
    # A fixed path: file browser rows show it, so it must not change between runs.
    root = ROOT
    shutil.rmtree(root, ignore_errors=True)
    (root / "logs").mkdir(parents=True)
    session = Session(args.binary.resolve(), root)
    failed = []
    try:
        session.start()
        for shot in shots:
            started = time.monotonic()
            try:
                image = session.capture(shot)
            except Exception as error:  # noqa: BLE001 - keep going so one broken shot doesn't stop the batch
                failed.append(shot.name)
                print(f"{shot.name}: FAILED: {error} (log: {root / 'logs' / shot.name}.log)", file=sys.stderr)
                continue
            target = args.out / f"{shot.name}.png"
            composite.save(image, target)
            print(f"{shot.name}: {image.width}x{image.height} {target.stat().st_size // 1024} KB "
                  f"({time.monotonic() - started:.1f}s)", flush=True)
    finally:
        session.stop()
        if args.keep or failed:
            print(f"temporary files kept in {root}", file=sys.stderr)
        else:
            shutil.rmtree(root, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
