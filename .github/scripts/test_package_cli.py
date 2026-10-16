"""Archive contract tests; no Rust toolchain or Android device required."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SPEC = importlib.util.spec_from_file_location("package_cli", Path(__file__).with_name("package-cli.py"))
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)


class PackageCliTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        previous = Path.cwd()
        os.chdir(self.temporary.name)
        self.addCleanup(os.chdir, previous)
        Path("target/release").mkdir(parents=True)
        Path("docs").mkdir()
        for name in ("ciallogcat-cli", "ciallogcat-cli.exe", "ciallogcat", "libciallogcat.rlib"):
            path = Path("target/release") / name
            path.write_bytes(b"fixture executable or library")
            path.chmod(0o755)
        Path("LICENSE").write_text("MIT fixture", encoding="utf-8")
        Path("docs/cli.md").write_text("CLI guide", encoding="utf-8")
        metadata = json.dumps({"packages": [{"name": "ciallogcat", "version": "1.0.0"}]})
        cargo = patch.object(PACKAGER.subprocess, "check_output", return_value=metadata)
        cargo.start()
        self.addCleanup(cargo.stop)

    def test_platform_archives_have_only_executable_license_and_guide(self):
        for platform, suffix, executable in (
            ("linux-x86_64", ".tar.gz", "ciallogcat-cli"),
            ("windows-x86_64", ".zip", "ciallogcat-cli.exe"),
        ):
            for tag in ("v1.0", "v1.0.0"):
                with self.subTest(platform=platform, tag=tag), patch.dict(
                    os.environ, RELEASE_TAG=tag, RELEASE_PLATFORM=platform
                ):
                    archive = PACKAGER.package()
                    root = f"ciallogcat-cli-{tag}-{platform}"
                    self.assertEqual(archive.name, root + suffix)
                    expected = {f"{root}/{name}" for name in (executable, "LICENSE", "README.md")}
                    if suffix == ".zip":
                        with zipfile.ZipFile(archive) as bundle:
                            self.assertEqual(set(bundle.namelist()), expected)
                            self.assertEqual(bundle.read(f"{root}/README.md"), b"CLI guide")
                    else:
                        with tarfile.open(archive) as bundle:
                            self.assertEqual(set(bundle.getnames()), expected)
                            self.assertTrue(bundle.getmember(f"{root}/{executable}").mode & 0o111)
                            self.assertEqual(bundle.extractfile(f"{root}/README.md").read(), b"CLI guide")
                    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                    self.assertEqual(
                        archive.with_name(archive.name + ".sha256").read_text(encoding="utf-8"),
                        f"{digest}  {archive.name}\n",
                    )

    def test_invalid_platform_tag_and_version_fail_closed(self):
        for tag, platform in (("v1.0", "macos-arm64"), ("../v1.0", "linux-x86_64"), ("v2.0", "windows-x86_64")):
            with self.subTest(tag=tag, platform=platform), patch.dict(
                os.environ, RELEASE_TAG=tag, RELEASE_PLATFORM=platform
            ), self.assertRaises(ValueError):
                PACKAGER.package()

    def test_missing_cli_does_not_fall_back_to_gui(self):
        Path("target/release/ciallogcat-cli").unlink()
        with patch.dict(os.environ, RELEASE_TAG="v1.0", RELEASE_PLATFORM="linux-x86_64"), self.assertRaises(FileNotFoundError):
            PACKAGER.package()


if __name__ == "__main__":
    unittest.main()
