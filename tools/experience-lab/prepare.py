#!/usr/bin/env python3
"""Prepare a Linux/Debian native experience lab without installing system packages.

Downloads distro packages into the chosen lab directory and extracts them there.
The GUI and CLI must already be built. The resulting directory is disposable.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
PACKAGES = [
    "xvfb", "xserver-common", "x11-xkb-utils", "libunwind8", "libxfont2",
    "libfontenc1", "libxkbfile1", "libxkbcommon-x11-0", "libxcb-xkb1",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gui", type=Path, required=True)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--lab-root", type=Path, default=ROOT / "target/experience-2026-10-23")
    parser.add_argument("--cached-packages", action="store_true",
                        help="extract packages already in the lab; do not access network")
    args = parser.parse_args()
    for executable in ("bwrap", "dpkg-deb", "apt-get"):
        if not shutil.which(executable):
            parser.error(f"required host tool is missing: {executable}")
    # Screenshot/clipboard integration uses the distro Python/Gtk bindings.
    subprocess.run(["/usr/bin/python3", "-c",
                    "import gi; gi.require_version('Gtk','3.0'); from gi.repository import Gtk"],
                   check=True)
    lab = args.lab_root.resolve()
    packages = lab / "packages"
    runtime = lab / "runtime"
    for directory in (packages, runtime, lab / "bin"):
        directory.mkdir(parents=True, exist_ok=True)
    if not args.cached_packages:
        subprocess.run(["apt-get", "download", *PACKAGES], cwd=packages, check=True, timeout=180)
    archives = sorted(packages.glob("*.deb"))
    missing = [name for name in PACKAGES if not any(p.name.startswith(name + "_") for p in archives)]
    if missing:
        parser.error("missing cached packages: " + ", ".join(missing))
    identities = {}
    for archive in archives:
        identity = tuple(subprocess.check_output(
            ["dpkg-deb", "-f", str(archive), field], text=True, timeout=10).strip()
            for field in ("Package", "Architecture"))
        if identity in identities:
            parser.error(f"multiple cached archives for {identity[0]}/{identity[1]}: "
                         f"{identities[identity].name}, {archive.name}; keep one version")
        identities[identity] = archive
    for archive in archives:
        subprocess.run(["dpkg-deb", "-x", str(archive), str(runtime)], check=True, timeout=30)
    (packages / "manifest.json").write_text(json.dumps(
        {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in archives}, indent=2) + "\n")

    # Xvfb invokes /usr/bin/xkbcomp by a compiled-in path. Make this available in
    # a private mount namespace. Resolve distro relative symlinks (e.g. env ->
    # ../lib/cargo/bin/coreutils/env) before relocating /usr/bin.
    merged = runtime / "merged-bin"
    host = runtime / "host-bin"
    merged.mkdir(exist_ok=True)
    host.mkdir(exist_ok=True)
    for source in Path("/usr/bin").iterdir():
        destination = merged / source.name
        if destination.is_symlink():
            destination.unlink()
        resolved = source.resolve()
        target = host / resolved.relative_to("/usr/bin") if resolved.is_relative_to("/usr/bin") else resolved
        destination.symlink_to(target)
    compiler = merged / "xkbcomp"
    if compiler.is_symlink():
        compiler.unlink()
    compiler.symlink_to(runtime / "usr/bin/xkbcomp")

    manifest = {}
    for name, source in (("ciallogcat", args.gui), ("ciallogcat-cli", args.cli)):
        destination = lab / "bin" / name
        if source.resolve() != destination:
            shutil.copy2(source, destination)
        manifest[name] = {"path": str(destination),
                          "sha256": hashlib.sha256(destination.read_bytes()).hexdigest()}
    (lab / "build-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"prepared": str(lab), "binaries": manifest}))


if __name__ == "__main__":
    main()
