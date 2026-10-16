#!/usr/bin/env python3
"""Package only the headless CLI; never include Cargo libraries or GUI assets."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import zipfile


def release_metadata() -> tuple[str, str, str]:
    tag = os.environ.get("RELEASE_TAG", "")
    platform = os.environ.get("RELEASE_PLATFORM", "")
    if platform not in ("linux-x86_64", "windows-x86_64"):
        raise ValueError("RELEASE_PLATFORM must be linux-x86_64 or windows-x86_64")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+(?:\.[0-9]+)?(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", tag):
        raise ValueError("RELEASE_TAG must be a version tag")
    # Match the existing GUI release's full/short version-tag rules.
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        text=True,
    ))
    version = next(item["version"] for item in metadata["packages"] if item["name"] == "ciallogcat")
    allowed_tags = {f"v{version}"}
    if re.fullmatch(r"[0-9]+\.[0-9]+\.0", version):
        allowed_tags.add(f"v{version.rsplit('.', 1)[0]}")
    if tag not in allowed_tags:
        raise ValueError(f"Tag {tag} must match Cargo version v{version}")

    return tag, platform, version


def package() -> Path:
    tag, platform, _ = release_metadata()
    name = f"ciallogcat-cli-{tag}-{platform}"
    executable = "ciallogcat-cli.exe" if platform == "windows-x86_64" else "ciallogcat-cli"
    output = Path("target/cli-release-dist")
    output.mkdir(parents=True, exist_ok=True)
    suffix = ".zip" if platform == "windows-x86_64" else ".tar.gz"
    archive = output / (name + suffix)
    with tempfile.TemporaryDirectory(prefix="ciallogcat-cli-") as temporary:
        stage = Path(temporary) / name
        stage.mkdir()
        shutil.copy2(Path("target/release") / executable, stage / executable)
        shutil.copy2("LICENSE", stage / "LICENSE")
        shutil.copy2("docs/cli.md", stage / "README.md")
        if platform == "windows-x86_64":
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
                for source in sorted(stage.iterdir()):
                    bundle.write(source, f"{name}/{source.name}")
        else:
            # Explicit allowlist also avoids packaging target/release/*.rlib.
            with tarfile.open(archive, "w:gz") as bundle:
                for source in sorted(stage.iterdir()):
                    info = bundle.gettarinfo(source, arcname=f"{name}/{source.name}")
                    # Set a portable Linux executable mode, even in archive tests
                    # running on Windows where chmod's execute bits are ignored.
                    info.mode = 0o755 if source.name == executable else 0o644
                    with source.open("rb") as stream:
                        bundle.addfile(info, stream)
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8"
    )
    return archive


def debian_version(version: str) -> str:
    base, separator, metadata = version.partition("+")
    return base.replace("-", "~", 1) + separator + metadata


def package_deb() -> Path:
    """Build a separate amd64 package from the headless ELF, without root."""
    tag, platform, version = release_metadata()
    if platform != "linux-x86_64":
        raise ValueError("CLI DEB packaging requires linux-x86_64")
    executable = Path("target/release/ciallogcat-cli").resolve()
    with executable.open("rb") as stream:
        header = stream.read(20)
    if (len(header) != 20 or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != 62):
        raise ValueError("CLI DEB requires an x86_64 little-endian ELF binary")
    output = Path("target/cli-release-dist")
    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"ciallogcat-cli-{tag}-{platform}.deb"
    with tempfile.TemporaryDirectory(prefix="ciallogcat-cli-deb-") as temporary:
        work = Path(temporary)
        (work / "debian").mkdir()
        # dpkg-shlibdeps expects source-package metadata even with -O.
        (work / "debian/control").write_text(
            "Source: ciallogcat-cli\nSection: devel\nPriority: optional\n"
            "Maintainer: IllyasvielEin <IllyasvielEin@users.noreply.github.com>\n\n"
            "Package: ciallogcat-cli\nArchitecture: amd64\n"
            "Description: Headless Android Logcat capture and query CLI\n",
            encoding="utf-8",
        )
        dependency_output = subprocess.check_output(
            ["dpkg-shlibdeps", "-O", "-e" + str(executable)], cwd=work, text=True,
        )
        lines = [line.removeprefix("shlibs:Depends=") for line in dependency_output.splitlines()
                 if line.startswith("shlibs:Depends=")]
        if len(lines) != 1 or not re.search(r"(?:^|, )libc6 \(>= [0-9][^)]*\)", lines[0]):
            raise ValueError("dpkg-shlibdeps must report a versioned libc6 dependency")
        depends = lines[0]
        root = work / "root"
        (root / "DEBIAN").mkdir(parents=True)
        (root / "usr/bin").mkdir(parents=True)
        documentation = root / "usr/share/doc/ciallogcat-cli"
        documentation.mkdir(parents=True)
        binary = root / "usr/bin/ciallogcat-cli"
        shutil.copyfile(executable, binary)
        binary.chmod(0o755)
        shutil.copyfile("LICENSE", documentation / "copyright")
        shutil.copyfile("docs/cli.md", documentation / "README.md")
        for document in documentation.iterdir():
            document.chmod(0o644)
        # Debian uses ~ for prerelease ordering; preserve +build metadata.
        deb_version = debian_version(version)
        (root / "DEBIAN/control").write_text(
            f"Package: ciallogcat-cli\nVersion: {deb_version}\nArchitecture: amd64\n"
            "Maintainer: IllyasvielEin <IllyasvielEin@users.noreply.github.com>\n"
            "Section: devel\nPriority: optional\n"
            f"Depends: {depends}\nRecommends: adb\n"
            "Homepage: https://github.com/IllyasvielEin/Ciallogcat\n"
            "Description: Headless Android Logcat capture and query CLI\n"
            " Bounded JSONL capture and local background sessions; no GUI required.\n"
            " ADB is external; Android SDK Platform-Tools can supply it instead.\n",
            encoding="utf-8",
        )
        subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(root),
                        str(archive.resolve())], check=True)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8",
    )
    return archive


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deb-only", action="store_true", help="build only the Linux CLI DEB")
    args = parser.parse_args()
    if not args.deb_only:
        print(package())
    if args.deb_only or os.environ.get("RELEASE_PLATFORM") == "linux-x86_64":
        print(package_deb())
