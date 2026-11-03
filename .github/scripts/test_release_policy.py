"""Release validation and actual workflow-gate regressions; no network required."""

import itertools
import json
from pathlib import Path
import re
import tempfile
import unittest

import release_policy as policy


class ReleaseFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "docs/releases").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text('[package]\nname = "ciallogcat"\nversion = "1.3.1"\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "ciallogcat"\nversion = "1.3.1"\n')
        self.plan = {"tag": "v1.3.1", "version": "1.3.1", "scope": "cli"}
        self.plan_file = self.root / "docs/releases/v1.3.1.json"
        self.plan_file.write_text(json.dumps(self.plan))
        self.notes = self.root / "docs/releases/v1.3.1.md"
        self.notes.write_text("CLI patch notes\n")
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.write_assets()

    def write_assets(self):
        for name in policy.expected_assets(self.plan):
            if not name.endswith(".sha256"):
                package = self.dist / name
                package.write_bytes(b"fixture " + name.encode())
                package.with_name(name + ".sha256").write_text(f"{policy.digest(package)}  {name}\n")


class ReleasePolicyTests(ReleaseFixture):
    def test_cli_and_full_asset_sets(self):
        self.assertEqual(policy.load_plan(self.root, "v1.3.1"), self.plan)
        self.assertEqual(len(policy.validate_assets(self.dist, self.plan)), 6)
        self.plan["scope"] = "full"
        self.plan_file.write_text(json.dumps(self.plan))
        self.write_assets()
        self.assertEqual(policy.load_plan(self.root, "v1.3.1"), self.plan)
        self.assertEqual(len(policy.validate_assets(self.dist, self.plan)), 12)

    def test_missing_invalid_or_mismatched_plan_fails(self):
        self.plan_file.unlink()
        with self.assertRaises(FileNotFoundError):
            policy.load_plan(self.root, "v1.3.1")
        for change in ({"scope": "gui"}, {"scope": None}, {"tag": "v1.3.0"},
                       {"version": "1.3.0"}, {"extra": True}):
            with self.subTest(change=change):
                self.plan_file.write_text(json.dumps(self.plan | change))
                with self.assertRaises(ValueError):
                    policy.load_plan(self.root, "v1.3.1")
        self.plan_file.write_text(json.dumps({"tag": "v1.3.1", "version": "1.3.1"}))
        with self.assertRaises(ValueError):
            policy.load_plan(self.root, "v1.3.1")

    def test_invalid_tag_lock_and_notes_fail(self):
        for tag in ("../v1.3.1", "v1.3.0", "v1.3", "1.3.1"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                policy.load_plan(self.root, tag)
        self.notes.write_text(" \n")
        with self.assertRaises(ValueError):
            policy.load_plan(self.root, "v1.3.1")
        self.notes.unlink()
        with self.assertRaises(FileNotFoundError):
            policy.load_plan(self.root, "v1.3.1")
        self.notes.write_text("notes")
        (self.root / "Cargo.lock").write_text('[[package]]\nname="ciallogcat"\nversion="1.3.0"')
        with self.assertRaises(ValueError):
            policy.load_plan(self.root, "v1.3.1")

    def test_missing_extra_wrong_version_and_gui_assets_fail(self):
        package = next(p for p in self.dist.iterdir() if not p.name.endswith(".sha256"))
        contents = package.read_bytes()
        package.unlink()
        with self.assertRaises(ValueError):
            policy.validate_assets(self.dist, self.plan)
        package.write_bytes(contents)
        for name in ("unexpected.txt", "ciallogcat-v1.3.1-linux-x86_64.deb",
                     "ciallogcat-cli-v1.3.0-linux-x86_64.deb"):
            extra = self.dist / name
            extra.write_text("unexpected")
            with self.subTest(name=name), self.assertRaises(ValueError):
                policy.validate_assets(self.dist, self.plan)
            extra.unlink()

    def test_checksums_cannot_name_other_files_or_hide_extra_lines(self):
        checksum = next(self.dist.glob("*.sha256"))
        original = checksum.read_text()
        for text in (original.replace(original[:64], "0" * 64), original + original,
                     original.replace("  ciallogcat", "  ../ciallogcat")):
            checksum.write_text(text)
            with self.assertRaises(ValueError):
                policy.validate_assets(self.dist, self.plan)
        checksum.write_text(original)
        package = checksum.with_name(checksum.name.removesuffix(".sha256"))
        package.write_bytes(b"tampered")
        with self.assertRaises(ValueError):
            policy.validate_assets(self.dist, self.plan)

    def test_asset_directory_is_rejected(self):
        package = next(p for p in self.dist.iterdir() if not p.name.endswith(".sha256"))
        package.unlink()
        package.mkdir()
        with self.assertRaises(ValueError):
            policy.validate_assets(self.dist, self.plan)


class WorkflowGateTests(unittest.TestCase):
    def test_actual_publish_expression_accepts_only_required_successes(self):
        workflow = Path(__file__).parents[1] / "workflows/ci.yml"
        release = workflow.read_text().split("\n  release:\n", 1)[1]
        expression = re.search(r"if: >-\s*\$\{\{(.*?)\}\}", release, re.S)[1]

        def accepts(scope, preflight, cli, build, cancelled=False, event="push", ref="refs/tags/v1.3.1"):
            evaluated = expression
            for key, value in {
                "needs.preflight.outputs.scope": scope,
                "needs.preflight.result": preflight,
                "needs.cli.result": cli,
                "needs.build.result": build,
                "github.event_name": event,
                "github.ref": ref,
            }.items():
                evaluated = evaluated.replace(key, repr(value))
            evaluated = evaluated.replace("always()", "True").replace("!cancelled()", repr(not cancelled))
            evaluated = evaluated.replace("&&", " and ").replace("||", " or ")
            return eval(" ".join(evaluated.split()), {"__builtins__": {}, "startsWith": str.startswith})

        states = ("success", "failure", "cancelled", "skipped")
        for scope in ("cli", "full", "invalid", ""):
            for preflight, cli, build in itertools.product(states, repeat=3):
                expected = preflight == cli == "success" and (
                    (scope == "cli" and build == "skipped") or
                    (scope == "full" and build == "success")
                )
                with self.subTest(scope=scope, preflight=preflight, cli=cli, build=build):
                    self.assertEqual(accepts(scope, preflight, cli, build), expected)
        for overrides in ({"cancelled": True}, {"event": "workflow_dispatch"}, {"ref": "refs/heads/main"}):
            self.assertFalse(accepts("cli", "success", "success", "skipped", **overrides))


if __name__ == "__main__":
    unittest.main()
