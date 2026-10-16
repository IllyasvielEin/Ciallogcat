#!/usr/bin/env python3
"""Package only the headless CLI; never include Cargo libraries or GUI assets."""

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


def package() -> Path:
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


if __name__ == "__main__":
    print(package())
