"""Publish verified artifacts from one immutable tag, using GitHub CLI."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.parse import quote

from release_policy import digest, expected_assets, load_plan, validate_assets


def gh(*args: str):
    subprocess.run(["gh", *args], check=True)


def api(path: str, *, missing_ok: bool = False):
    result = subprocess.run(
        ["gh", "api", "--include", "--method", "GET", path],
        text=True, capture_output=True,
    )
    headers, separator, body = result.stdout.replace("\r\n", "\n").partition("\n\n")
    status = re.match(r"HTTP/\S+ (\d{3})\b", headers)
    code = int(status[1]) if status else None
    if code == 404 and missing_ok:
        return None
    if result.returncode or code != 200 or not separator:
        raise RuntimeError(f"GitHub API request failed ({code}): {result.stderr.strip()}")
    return json.loads(body)


def verify_tag(repo: str, tag: str, commit: str):
    obj = api(f"repos/{repo}/git/ref/tags/{quote(tag, safe='')}")["object"]
    for _ in range(10):
        if obj["type"] == "commit":
            if obj["sha"] != commit:
                raise ValueError("remote release tag does not match the build commit")
            return
        if obj["type"] != "tag":
            break
        obj = api(f"repos/{repo}/git/tags/{obj['sha']}")["object"]
    raise ValueError("release tag does not resolve to a commit")


def check_draft(release: dict):
    if release.get("draft") is not True:
        raise ValueError("release is already public; published assets will not be overwritten")


def find_release(repo: str, tag: str):
    # GET /releases/tags/{tag} only finds published releases. Authenticated
    # listing includes drafts; use the release ID for every subsequent read.
    matches = []
    page = 1
    while True:
        releases = api(f"repos/{repo}/releases?per_page=100&page={page}")
        matches.extend(release for release in releases if release["tag_name"] == tag)
        if len(releases) < 100:
            break
        page += 1
    if len(matches) > 1:
        raise ValueError("multiple releases use the requested tag")
    return matches[0] if matches else None


def check_remote_assets(repo: str, release: dict, expected: set[str], *, complete: bool):
    # At most 12 assets are valid. A first page of 100 is already an error.
    assets = api(f"repos/{repo}/releases/{release['id']}/assets?per_page=100")
    names = [asset["name"] for asset in assets]
    if len(names) != len(set(names)) or not set(names) <= expected:
        raise ValueError("remote draft contains duplicate or unexpected assets")
    if complete and set(names) != expected:
        raise ValueError("remote draft is missing release assets")


def publish(root: Path, directory: Path, tag: str, repo: str, commit: str):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("invalid GitHub repository")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("invalid build commit")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if head != commit:
        raise ValueError("checkout does not match the build commit")
    plan = load_plan(root, tag)
    paths = validate_assets(directory, plan)
    expected = expected_assets(plan)
    verify_tag(repo, tag, commit)
    latest_endpoint = f"repos/{repo}/releases/latest"
    previous_latest = api(latest_endpoint, missing_ok=True)
    release = find_release(repo, tag)
    component = {"gui": " GUI", "cli": " CLI", "full": ""}[plan["scope"]]
    title = f"Ciallogcat{component} v{plan['version']}"
    notes = str(root / "docs/releases" / f"{tag}.md")
    if release is None:
        gh("release", "create", tag, "--repo", repo, "--verify-tag", "--draft",
           "--latest=false", "--title", title, "--notes-file", notes)
        release = find_release(repo, tag)
        if release is None:
            raise ValueError("created release draft is not visible")
    endpoint = f"repos/{repo}/releases/{release['id']}"
    check_draft(release)
    check_remote_assets(repo, release, expected, complete=False)
    gh("release", "edit", tag, "--repo", repo, "--title", title, "--notes-file", notes)
    gh("release", "upload", tag, "--repo", repo, *(str(path) for path in paths), "--clobber")
    check_remote_assets(repo, release, expected, complete=True)
    with tempfile.TemporaryDirectory(prefix="ciallogcat-release-verify-") as temporary:
        downloaded = Path(temporary)
        gh("release", "download", tag, "--repo", repo, "--dir", str(downloaded))
        validate_assets(downloaded, plan)
        if any(digest(downloaded / path.name) != digest(path) for path in paths):
            raise ValueError("remote draft content does not match the build artifacts")
    verify_tag(repo, tag, commit)
    check_draft(api(endpoint))
    check_remote_assets(repo, release, expected, complete=True)
    prerelease = "-" in plan["version"].split("+", 1)[0]
    latest = plan["scope"] in ("gui", "full") and not prerelease
    gh("release", "edit", tag, "--repo", repo, "--draft=false",
       f"--prerelease={str(prerelease).lower()}", f"--latest={str(latest).lower()}")
    published = api(endpoint)
    if (published.get("draft") is not False or published.get("prerelease") != prerelease
            or published.get("name") != title
            or published.get("body", "").strip() != Path(notes).read_text(encoding="utf-8").strip()):
        raise ValueError("published release metadata does not match the release plan")
    check_remote_assets(repo, published, expected, complete=True)
    current_latest = api(latest_endpoint, missing_ok=True)
    expected_latest = published if latest else previous_latest
    if (current_latest or {}).get("id") != (expected_latest or {}).get("id"):
        raise ValueError("published release has an unexpected Latest marker")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"), required=not os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args()
    publish(args.root.resolve(), args.dist.resolve(), args.tag, args.repo, args.commit)


if __name__ == "__main__":
    main()
