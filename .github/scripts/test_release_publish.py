"""Exercise draft recovery and the final publication boundary with a fake API."""

import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

from release_policy import digest, expected_assets
from test_release_policy import ReleaseFixture

SPEC = importlib.util.spec_from_file_location("publish_release", Path(__file__).with_name("publish-release.py"))
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


class PublishTests(ReleaseFixture):
    def setUp(self):
        super().setUp()
        self.commit = "a" * 40
        self.release = {"id": 7, "draft": True, "tag_name": "v1.3.1"}
        self.remote = {}
        self.commands = []
        self.tag_commit = self.commit
        self.corrupt_download = False
        self.latest = {"id": 6}
        for patcher in (
            patch.object(publisher, "api", side_effect=self.api),
            patch.object(publisher, "gh", side_effect=self.gh),
            patch.object(publisher.subprocess, "check_output", return_value=self.commit + "\n"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def api(self, path, *, missing_ok=False):
        if path.endswith("/releases/latest"):
            return self.latest
        if "/releases?" in path:
            return [self.release] if self.release else []
        if "/releases/tags/" in path:
            raise AssertionError("drafts must be read by ID, not by tag")
        if "/git/ref/tags/" in path:
            return {"object": {"type": "tag", "sha": "b" * 40}}
        if "/git/tags/" in path:
            return {"object": {"type": "commit", "sha": self.tag_commit}}
        if "/assets?" in path:
            return [{"name": name} for name in self.remote]
        if self.release is None and not missing_ok:
            raise RuntimeError("not found")
        return self.release

    def gh(self, *args):
        self.commands.append(args)
        if args[1] == "create":
            self.release = {"id": 7, "draft": True, "tag_name": args[2]}
        elif args[1] == "upload":
            for path in self.dist.iterdir():
                self.remote[path.name] = path.read_bytes()
        elif args[1] == "download":
            directory = Path(args[args.index("--dir") + 1])
            for name, content in self.remote.items():
                (directory / name).write_bytes(content)
            if self.corrupt_download:
                # Even a self-consistent replacement must differ from our build.
                package = next(p for p in directory.iterdir() if not p.name.endswith(".sha256"))
                package.write_bytes(b"different build")
                package.with_name(package.name + ".sha256").write_text(f"{digest(package)}  {package.name}\n")
        elif "--draft=false" in args:
            self.release["draft"] = False
            self.release["prerelease"] = "--prerelease=true" in args
            if "--latest=true" in args:
                self.latest = self.release
        if "--title" in args:
            self.release["name"] = args[args.index("--title") + 1]
        if "--notes-file" in args:
            self.release["body"] = Path(args[args.index("--notes-file") + 1]).read_text()

    def publish(self):
        publisher.publish(self.root, self.dist, "v1.3.1", "owner/repo", self.commit)

    def assert_not_published(self):
        self.assertFalse(any("--draft=false" in args for args in self.commands))

    def test_new_cli_release_is_created_as_draft_and_keeps_latest(self):
        self.release = None
        self.publish()
        self.assertEqual(self.commands[0][1], "create")
        self.assertIn("--draft", self.commands[0])
        self.assertIn("--verify-tag", self.commands[0])
        self.assertIn("--latest=false", self.commands[-1])
        self.assertIn("--prerelease=false", self.commands[-1])
        self.assertEqual(set(self.remote), expected_assets(self.plan))
        upload = next(args for args in self.commands if args[1] == "upload")
        self.assertEqual({Path(arg).name for arg in upload if arg.startswith(str(self.dist))}, expected_assets(self.plan))
        self.assertFalse(any("--generate-notes" in args for args in self.commands))

    def test_partial_draft_can_be_resumed(self):
        self.remote[next(iter(expected_assets(self.plan)))] = b"old incomplete upload"
        self.publish()
        self.assertFalse(any(args[1] == "create" for args in self.commands))
        self.assertFalse(self.release["draft"])

    def test_public_release_and_unexpected_draft_assets_are_not_modified(self):
        self.release["draft"] = False
        with self.assertRaisesRegex(ValueError, "already public"):
            self.publish()
        self.assertEqual(self.commands, [])
        self.release["draft"] = True
        self.remote["ciallogcat-v1.3.1-linux-x86_64.deb"] = b"stale GUI asset"
        with self.assertRaisesRegex(ValueError, "unexpected"):
            self.publish()
        self.assertEqual(self.commands, [])

    def test_full_release_contains_all_assets_and_becomes_latest(self):
        self.plan["scope"] = "full"
        self.plan_file.write_text(json.dumps(self.plan))
        self.write_assets()
        self.publish()
        self.assertEqual(len(self.remote), 12)
        self.assertIn("--latest=true", self.commands[-1])
        self.assertEqual(self.latest["id"], 7)

    def test_published_latest_is_verified(self):
        original = self.gh

        def change_latest(*args):
            original(*args)
            if "--draft=false" in args:
                self.latest = self.release
        with patch.object(publisher, "gh", side_effect=change_latest):
            with self.assertRaisesRegex(ValueError, "Latest"):
                self.publish()

    def test_tag_moved_after_upload_cannot_be_published(self):
        original = self.gh

        def move_tag(*args):
            original(*args)
            if args[1] == "download":
                self.tag_commit = "c" * 40

        with patch.object(publisher, "gh", side_effect=move_tag):
            with self.assertRaisesRegex(ValueError, "build commit"):
                self.publish()
        self.assert_not_published()

    def test_prerelease_does_not_replace_latest(self):
        version = "1.4.0-rc.1"
        tag = f"v{version}"
        self.plan = {"tag": tag, "version": version, "scope": "full"}
        self.release["tag_name"] = tag
        for manifest in ("Cargo.toml", "Cargo.lock"):
            path = self.root / manifest
            path.write_text(path.read_text().replace("1.3.1", version))
        (self.root / f"docs/releases/{tag}.json").write_text(json.dumps(self.plan))
        (self.root / f"docs/releases/{tag}.md").write_text("Prerelease notes")
        for path in self.dist.iterdir():
            path.unlink()
        self.write_assets()
        publisher.publish(self.root, self.dist, tag, "owner/repo", self.commit)
        self.assertIn("--prerelease=true", self.commands[-1])
        self.assertIn("--latest=false", self.commands[-1])
        self.assertEqual(self.latest["id"], 6)

    def test_moved_tag_prevents_any_release_mutation(self):
        self.tag_commit = "c" * 40
        with self.assertRaisesRegex(ValueError, "build commit"):
            self.publish()
        self.assertEqual(self.commands, [])

    def test_corrupt_remote_content_cannot_be_published(self):
        self.corrupt_download = True
        with self.assertRaisesRegex(ValueError, "does not match"):
            self.publish()
        self.assert_not_published()

    def test_missing_upload_cannot_be_published(self):
        def skip_upload(*args):
            if args[1] != "upload":
                self.gh(*args)
        with patch.object(publisher, "gh", side_effect=skip_upload):
            with self.assertRaisesRegex(ValueError, "missing release assets"):
                self.publish()
        self.assert_not_published()

    def test_failed_upload_cannot_be_published(self):
        def fail_upload(*args):
            if args[1] == "upload":
                raise subprocess.CalledProcessError(1, ["gh", *args])
            self.gh(*args)
        with patch.object(publisher, "gh", side_effect=fail_upload):
            with self.assertRaises(subprocess.CalledProcessError):
                self.publish()
        self.assert_not_published()


class ApiTests(unittest.TestCase):
    def test_draft_discovery_paginates_and_rejects_ambiguous_tags(self):
        draft = {"id": 7, "tag_name": "v1.3.1", "draft": True}
        first_page = [{"id": n, "tag_name": f"v0.0.{n}"} for n in range(100)]
        with patch.object(publisher, "api", side_effect=[first_page, [draft]]) as api:
            self.assertEqual(publisher.find_release("owner/repo", "v1.3.1"), draft)
            self.assertIn("page=2", api.call_args.args[0])
        with patch.object(publisher, "api", return_value=[draft, draft]):
            with self.assertRaisesRegex(ValueError, "multiple releases"):
                publisher.find_release("owner/repo", "v1.3.1")
        with patch.object(publisher, "api", side_effect=RuntimeError("API failure")):
            with self.assertRaisesRegex(RuntimeError, "API failure"):
                publisher.find_release("owner/repo", "v1.3.1")

    def test_only_an_http_404_means_missing_release(self):
        for code in (401, 403, 500):
            response = subprocess.CompletedProcess([], 1, f"HTTP/2.0 {code} Error\n\n{{}}", "API error")
            with patch.object(publisher.subprocess, "run", return_value=response):
                with self.assertRaises(RuntimeError):
                    publisher.api("release", missing_ok=True)
        response = subprocess.CompletedProcess([], 1, "", "error connecting to api.github.com")
        with patch.object(publisher.subprocess, "run", return_value=response):
            with self.assertRaises(RuntimeError):
                publisher.api("release", missing_ok=True)
        response = subprocess.CompletedProcess([], 1, 'HTTP/2.0 404 Not Found\r\n\r\n{}', "not found")
        with patch.object(publisher.subprocess, "run", return_value=response):
            self.assertIsNone(publisher.api("release", missing_ok=True))
            with self.assertRaises(RuntimeError):
                publisher.api("release")

    def test_successful_api_response_is_decoded(self):
        response = subprocess.CompletedProcess([], 0, 'HTTP/2.0 200 OK\nContent-Type: application/json\n\n{"draft":true}', "")
        with patch.object(publisher.subprocess, "run", return_value=response):
            self.assertEqual(publisher.api("release"), {"draft": True})


if __name__ == "__main__":
    unittest.main()
