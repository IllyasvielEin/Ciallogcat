#!/usr/bin/env python3
"""Linux native-window startup smoke test, using only Python's standard library.

CI supplies an isolated X display with xvfb-run. This script only reads X11
window metadata; it never focuses, closes, resizes, or sends input to a window.
It terminates only the GUI child it started. A mapped window proves startup,
not rendering correctness, desktop integration, IME, or Android connectivity.

    xvfb-run -a env WINIT_UNIX_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 \
        python3 -B tools/native-gui-smoke.py --binary target/debug/ciallogcat

Each run keeps stderr/stdout, fake-ADB trace and result.json under the artifact
directory, including on failure. The test uses a separate temporary config and
the no_devices fixture; it never invokes a real ADB installation.
"""
from __future__ import annotations

import argparse
from collections import deque
import ctypes as C
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
STARTUP_SECONDS = 15.0
SETTLE_SECONDS = 0.3
Window = C.c_ulong


class WindowAttributes(C.Structure):
    _fields_ = [
        (name, kind) for name, kind in (
            ("x", C.c_int), ("y", C.c_int), ("width", C.c_int),
            ("height", C.c_int), ("border_width", C.c_int), ("depth", C.c_int),
            ("visual", C.c_void_p), ("root", Window), ("window_class", C.c_int),
            ("bit_gravity", C.c_int), ("win_gravity", C.c_int),
            ("backing_store", C.c_int), ("backing_planes", C.c_ulong),
            ("backing_pixel", C.c_ulong), ("save_under", C.c_int),
            ("colormap", C.c_ulong), ("map_installed", C.c_int),
            ("map_state", C.c_int), ("all_event_masks", C.c_long),
            ("your_event_mask", C.c_long), ("do_not_propagate_mask", C.c_long),
            ("override_redirect", C.c_int), ("screen", C.c_void_p),
        )
    ]


class Display:
    def __init__(self, name: str):
        self.x = C.CDLL("libX11.so.6")
        self.x.XOpenDisplay.argtypes = [C.c_char_p]
        self.x.XOpenDisplay.restype = C.c_void_p
        self.x.XCloseDisplay.argtypes = [C.c_void_p]
        self.x.XDefaultRootWindow.argtypes = [C.c_void_p]
        self.x.XDefaultRootWindow.restype = Window
        self.x.XInternAtom.argtypes = [C.c_void_p, C.c_char_p, C.c_int]
        self.x.XInternAtom.restype = C.c_ulong
        self.x.XQueryTree.argtypes = [
            C.c_void_p, Window, C.POINTER(Window), C.POINTER(Window),
            C.POINTER(C.POINTER(Window)), C.POINTER(C.c_uint),
        ]
        self.x.XGetWindowAttributes.argtypes = [C.c_void_p, Window, C.POINTER(WindowAttributes)]
        self.x.XGetWindowProperty.argtypes = [
            C.c_void_p, Window, C.c_ulong, C.c_long, C.c_long, C.c_int,
            C.c_ulong, C.POINTER(C.c_ulong), C.POINTER(C.c_int),
            C.POINTER(C.c_ulong), C.POINTER(C.c_ulong), C.POINTER(C.POINTER(C.c_ubyte)),
        ]
        self.x.XFree.argtypes = [C.c_void_p]
        # Windows may disappear between QueryTree/property/attributes requests.
        # Ignore asynchronous X errors and use each operation's checked status.
        self.error_callback = C.CFUNCTYPE(C.c_int, C.c_void_p, C.c_void_p)(lambda _d, _e: 0)
        self.x.XSetErrorHandler.argtypes = [type(self.error_callback)]
        self.x.XSetErrorHandler.restype = C.c_void_p
        self.x.XSetErrorHandler(self.error_callback)
        self.handle = self.x.XOpenDisplay(name.encode())
        if not self.handle:
            raise RuntimeError(f"cannot connect to DISPLAY={name}; run under xvfb-run")

    def close(self) -> None:
        if self.handle:
            self.x.XCloseDisplay(self.handle)
            self.handle = None

    def pid(self, window: int, atom: int) -> int | None:
        kind, width = C.c_ulong(), C.c_int()
        count, remaining = C.c_ulong(), C.c_ulong()
        data = C.POINTER(C.c_ubyte)()
        try:
            status = self.x.XGetWindowProperty(
                self.handle, window, atom, 0, 1, 0, 6,  # XA_CARDINAL
                C.byref(kind), C.byref(width), C.byref(count),
                C.byref(remaining), C.byref(data),
            )
            if status == 0 and kind.value == 6 and width.value == 32 and count.value == 1 and data:
                # Xlib represents 32-bit properties as native unsigned longs.
                return int(C.cast(data, C.POINTER(C.c_ulong))[0])
            return None
        finally:
            if data:
                self.x.XFree(data)

    def mapped_window(self, pid: int) -> int | None:
        atom = self.x.XInternAtom(self.handle, b"_NET_WM_PID", 1)
        if not atom:
            return None
        queue = deque([(self.x.XDefaultRootWindow(self.handle), 0)])
        visited = 0
        while queue and visited < 4096:
            window, depth = queue.popleft()
            visited += 1
            if self.pid(window, atom) == pid:
                attributes = WindowAttributes()
                if (self.x.XGetWindowAttributes(self.handle, window, C.byref(attributes))
                        and attributes.map_state == 2  # IsViewable
                        and attributes.width > 0 and attributes.height > 0):
                    return int(window)
            if depth >= 8:
                continue
            root, parent, count = Window(), Window(), C.c_uint()
            children = C.POINTER(Window)()
            try:
                if self.x.XQueryTree(self.handle, window, C.byref(root), C.byref(parent),
                                     C.byref(children), C.byref(count)):
                    for index in range(min(count.value, 4096 - visited - len(queue))):
                        queue.append((children[index], depth + 1))
            finally:
                if children:
                    self.x.XFree(children)
        return None


def stop(child: subprocess.Popen) -> None:
    if child.poll() is not None:
        child.wait()
        return
    child.terminate()
    try:
        child.wait(timeout=2)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/ciallogcat")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/native-gui-smoke")
    args = parser.parse_args()
    args.artifacts.mkdir(parents=True, exist_ok=True)
    artifacts = Path(tempfile.mkdtemp(prefix="run-", dir=args.artifacts.resolve()))
    result = {"status": "failed", "artifacts": str(artifacts), "timeout_seconds": STARTUP_SECONDS}
    child = None
    display = None
    started = time.monotonic()
    try:
        if not sys.platform.startswith("linux"):
            raise RuntimeError("this smoke test requires Linux and an X11 display")
        name = os.environ.get("DISPLAY", "")
        if not name.startswith(":"):
            raise RuntimeError("a local DISPLAY such as :99 is required; use xvfb-run -a")
        binary = args.binary.resolve(strict=True)
        display = Display(name)
        with tempfile.TemporaryDirectory(prefix="scratch-", dir=artifacts) as scratch:
            scratch = Path(scratch)
            fixture = scratch / "fake-adb"
            shutil.copyfile(ROOT / "tests/fixtures/fake_adb.py", fixture)
            fixture.chmod(0o700)
            environment = dict(os.environ, CIALLOGCAT_ADB=str(fixture),
                CIALLOGCAT_CONFIG_DIR=str(scratch / "config"),
                CIALLOGCAT_FAKE_SCENARIO="no_devices",
                CIALLOGCAT_FAKE_TRACE=str(artifacts / "adb-trace.jsonl"),
                WINIT_UNIX_BACKEND="x11", LIBGL_ALWAYS_SOFTWARE="1",
                PYTHONDONTWRITEBYTECODE="1")
            # Do not let a developer's inherited Wayland endpoint bypass the
            # explicitly selected Xvfb/X11 display in versions that prefer it.
            environment.pop("WAYLAND_DISPLAY", None)
            environment.pop("WAYLAND_SOCKET", None)
            with (artifacts / "stderr.log").open("wb") as stderr, (artifacts / "stdout.log").open("wb") as stdout:
                child = subprocess.Popen([str(binary)], env=environment,
                    stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
                result.update(pid=child.pid, binary=str(binary), display=name)
                try:
                    mapped_since = None
                    while time.monotonic() - started < STARTUP_SECONDS:
                        if child.poll() is not None:
                            raise RuntimeError(f"GUI exited before stable mapped window: {child.returncode}")
                        window = display.mapped_window(child.pid)
                        if window is not None:
                            mapped_since = mapped_since or time.monotonic()
                            if time.monotonic() - mapped_since >= SETTLE_SECONDS:
                                result.update(status="passed", window=f"0x{window:x}")
                                break
                        else:
                            mapped_since = None
                        time.sleep(0.05)
                    else:
                        raise TimeoutError("GUI did not map its own X11 window within 15 seconds")
                finally:
                    stop(child)
    except (OSError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        result.update(status="failed", error=str(error))
    finally:
        if child is not None and child.poll() is None:
            stop(child)
        if display is not None:
            display.close()
        result["elapsed_seconds"] = round(time.monotonic() - started, 3)
        if child is not None:
            result["child_exit_code"] = child.returncode
        (artifacts / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result))
    if result["status"] != "passed":
        stderr_path = artifacts / "stderr.log"
        if stderr_path.exists():
            with stderr_path.open("rb") as stderr:
                stderr.seek(max(0, stderr_path.stat().st_size - 8192))
                print(stderr.read().decode("utf-8", errors="replace"), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
