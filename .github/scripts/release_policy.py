"""Validate the versioned release plan and the exact set of publishable files."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tomllib


def load_plan(root: Path, tag: str) -> dict:
    match = re.fullmatch(r"(?:(gui|cli)-)?v([0-9]+\.[0-9]+(?:\.[0-9]+)?(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)", tag)
    if not match:
        raise ValueError("invalid release tag")
    component = match[1]
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    if component:
        if component == "cli":
            manifest = tomllib.loads((root / "crates/cli/Cargo.toml").read_text(encoding="utf-8"))
    elif "workspace" in manifest:
        raise ValueError("workspace releases require gui-v or cli-v tags")
    package = manifest["package"]
    version = package["version"]
    allowed = {version}
    if re.fullmatch(r"[0-9]+\.[0-9]+\.0", version):
        allowed.add(version.rsplit(".", 1)[0])
    if match[2] not in allowed:
        raise ValueError("release tag does not match Cargo version")
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    if [p["version"] for p in lock["package"] if p["name"] == package["name"]] != [version]:
        raise ValueError("Cargo.lock does not match Cargo version")
    plan = json.loads((root / "docs/releases" / f"{tag}.json").read_text(encoding="utf-8"))
    if not isinstance(plan, dict) or set(plan) != {"tag", "version", "scope"}:
        raise ValueError("release plan must contain exactly tag, version and scope")
    if plan["tag"] != tag or plan["scope"] not in ("gui", "cli", "full"):
        raise ValueError("release plan has an invalid tag, version or scope")
    if plan["version"] != version or (component and plan["scope"] != component):
        raise ValueError("tag component/version does not match release plan")
    if not component and plan["scope"] == "gui":
        raise ValueError("GUI-only releases require a gui-v tag")
    notes = root / "docs/releases" / f"{tag}.md"
    if not notes.read_text(encoding="utf-8").strip():
        raise ValueError("release notes must not be empty")
    return plan


def asset_tag(plan: dict) -> str:
    # Component is already present in the executable/package name.
    return re.sub(r"^(gui|cli)-", "", plan["tag"])


def expected_assets(plan: dict) -> set[str]:
    prefixes = {"gui": ["ciallogcat"], "cli": ["ciallogcat-cli"],
                "full": ["ciallogcat", "ciallogcat-cli"]}.get(plan["scope"])
    if prefixes is None:
        raise ValueError("unsupported release scope")
    packages = {
        f"{prefix}-{asset_tag(plan)}-{platform}.{extension}"
        for prefix in prefixes
        for platform, extension in (
            ("linux-x86_64", "tar.gz"),
            ("windows-x86_64", "zip"),
            ("linux-x86_64", "deb"),
        )
    }
    return packages | {name + ".sha256" for name in packages}


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_assets(directory: Path, plan: dict) -> list[Path]:
    expected = expected_assets(plan)
    actual = {path.name for path in directory.iterdir()}
    if actual != expected:
        raise ValueError(f"release asset mismatch: missing={sorted(expected - actual)}, unexpected={sorted(actual - expected)}")
    paths = [directory / name for name in sorted(expected)]
    if any(path.is_symlink() or not path.is_file() or path.stat().st_size == 0 for path in paths):
        raise ValueError("release assets must be nonempty regular files")
    for path in paths:
        if path.name.endswith(".sha256"):
            package = path.with_name(path.name.removesuffix(".sha256"))
            # One digest, one exact basename; no paths, wildcards or extra lines.
            checksum = path.read_text(encoding="utf-8")
            if checksum != f"{digest(package)}  {package.name}\n":
                raise ValueError(f"invalid SHA-256 file: {path.name}")
    return paths


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    plan = load_plan(args.root, args.tag)
    if output := os.environ.get("GITHUB_OUTPUT"):
        with Path(output).open("a", encoding="utf-8") as stream:
            for key in ("scope", "version"):
                stream.write(f"{key}={plan[key]}\n")
    print(json.dumps(plan))


if __name__ == "__main__":
    main()
