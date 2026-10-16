"""Archive contract tests; no Rust toolchain or Android device required."""

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
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


    def test_debian_version_preserves_build_metadata(self):
        for version, expected in (
            ("1.0.0", "1.0.0"),
            ("1.0.0-rc.1+build-foo", "1.0.0~rc.1+build-foo"),
            ("1.0.0+build-foo", "1.0.0+build-foo"),
        ):
            with self.subTest(version=version):
                self.assertEqual(PACKAGER.debian_version(version), expected)

    def test_deb_rejects_non_elf_and_windows(self):
        with patch.dict(os.environ, RELEASE_TAG="v1.0", RELEASE_PLATFORM="linux-x86_64"), self.assertRaises(ValueError):
            PACKAGER.package_deb()
        with patch.dict(os.environ, RELEASE_TAG="v1.0", RELEASE_PLATFORM="windows-x86_64"), self.assertRaises(ValueError):
            PACKAGER.package_deb()

    def elf_fixture(self):
        # Metadata fixture, not a runnable executable; real dependency resolution
        # and runtime/install checks run in Ubuntu CI without mocks.
        header = bytearray(20)
        header[:6] = b"\x7fELF\x02\x01"
        header[18:20] = (62).to_bytes(2, "little")
        Path("target/release/ciallogcat-cli").write_bytes(header)

    def test_deb_requires_verified_versioned_libc_dependency(self):
        self.elf_fixture()
        for dependencies in ("", "shlibs:Depends=\n", "shlibs:Depends=libc6, libgcc-s1\n"):
            with self.subTest(dependencies=dependencies), patch.dict(
                os.environ, RELEASE_TAG="v1.0", RELEASE_PLATFORM="linux-x86_64"
            ), patch.object(PACKAGER, "release_metadata", return_value=("v1.0", "linux-x86_64", "1.0.0")), patch.object(
                PACKAGER.subprocess, "check_output", return_value=dependencies
            ), self.assertRaises(ValueError):
                PACKAGER.package_deb()

    def test_deb_dependency_resolution_failure_is_not_ignored(self):
        self.elf_fixture()
        with patch.object(PACKAGER, "release_metadata", return_value=("v1.0", "linux-x86_64", "1.0.0")), patch.object(
            PACKAGER.subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, "dpkg-shlibdeps")
        ), self.assertRaises(subprocess.CalledProcessError):
            PACKAGER.package_deb()

    @unittest.skipUnless(shutil.which("dpkg-deb"), "Debian archive tool required")
    def test_deb_metadata_allowlist_ownership_and_checksum(self):
        self.elf_fixture()
        # Bypass the setUp Cargo mock to inspect actual archive-tool output.
        real_output = subprocess.run
        def dependency_fixture(command, *, cwd, text):
            self.assertEqual(command[0], "dpkg-shlibdeps")
            self.assertIn("-O", command)
            self.assertIn("Package: ciallogcat-cli", (Path(cwd) / "debian/control").read_text())
            return "shlibs:Depends=libc6 (>= 2.39), libgcc-s1 (>= 4.2)\n"
        with patch.object(PACKAGER, "release_metadata", return_value=("v1.0", "linux-x86_64", "1.0.0")), patch.object(
            PACKAGER.subprocess, "check_output", side_effect=dependency_fixture
        ) as resolve:
            archive = PACKAGER.package_deb()
            self.assertEqual(resolve.call_args.args[0][0], "dpkg-shlibdeps")
        def inspect(*args):
            return real_output(["dpkg-deb", *args, str(archive)], check=True, capture_output=True, text=True).stdout
        control = inspect("--field")
        for field in ("Package: ciallogcat-cli", "Version: 1.0.0", "Architecture: amd64", "Recommends: adb",
                      "Depends: libc6 (>= 2.39), libgcc-s1 (>= 4.2)"):
            self.assertIn(field, control)
        listing = inspect("--contents")
        files = [line for line in listing.splitlines() if not line.startswith("d")]
        self.assertEqual({line.split()[-1] for line in files}, {
            "./usr/bin/ciallogcat-cli", "./usr/share/doc/ciallogcat-cli/copyright",
            "./usr/share/doc/ciallogcat-cli/README.md",
        })
        self.assertTrue(all("root/root" in line for line in files))
        self.assertIn("-rwxr-xr-x", next(line for line in files if line.endswith("/ciallogcat-cli")))
        self.assertEqual(archive.name, "ciallogcat-cli-v1.0-linux-x86_64.deb")
        self.assertEqual(archive.with_name(archive.name + ".sha256").read_text(),
                         f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")


if __name__ == "__main__":
    unittest.main()
