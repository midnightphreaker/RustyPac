"""Focused release invariants; no network, credential access or container runtime."""

import hashlib
import importlib.util
import io
import json
import os
import struct
import subprocess
import tarfile
import tempfile
import unittest
import urllib.error
from email.message import Message
from pathlib import Path
from typing import Any
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
assert spec is not None and spec.loader is not None
r = importlib.util.module_from_spec(spec)
spec.loader.exec_module(r)
spec = importlib.util.spec_from_file_location("package", Path(__file__).with_name("package.py"))
assert spec is not None and spec.loader is not None
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


class ReleaseTests(unittest.TestCase):
    def test_container_url_identifies_versioned_lowercase_image(self):
        self.assertEqual(
            r.container_url("https://git.phrk.org/", "pub/HoomanBrowser", "0.0.3"),
            "https://git.phrk.org/pub/-/packages/container/hoomanbrowser/v0.0.3",
        )
        self.assertEqual(
            r.container_url("https://forgejo.invalid", "Owner Name/Mixed+Repo", "1.2.3+meta"),
            "https://forgejo.invalid/Owner%20Name/-/packages/container/mixed%2Brepo/v1.2.3%2Bmeta",
        )

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.old = Path.cwd()
        os.chdir(self.tmp.name)
        Path("VERSION").write_text("0.1.7\n")
        Path("project.toml").write_text('version = "0.1.7"\ndep = "9.1.2"\n')
        self.config: dict[str, Any] = {
            "version_files": [
                {"path": "project.toml", "pattern": '^version = "(?P<version>[^"]+)"'}
            ]
        }

    def tearDown(self):
        os.chdir(self.old)
        self.tmp.cleanup()

    def test_bumps_and_prerelease(self):
        self.assertEqual(r.bump("0.1.7", "Major"), "0.2.0")
        self.assertEqual(r.bump("0.1.7", "minor"), "0.1.8")
        self.assertEqual(r.bump("0.10.3-playtest", "retry"), "0.10.3-playtest")
        self.assertEqual(r.bump("0.10.3-playtest", "minor"), "0.10.4")
        for invalid in ("v0.1.7", "0.1", "01.1.0", "0.1.7; exit"):
            with self.assertRaises(ValueError):
                r.version(invalid)

    def test_sync_changes_only_owned_version(self):
        r.sync_versions(self.config, "0.2.0")
        self.assertEqual(Path("VERSION").read_text(), "0.2.0\n")
        self.assertEqual(Path("project.toml").read_text(), 'version = "0.2.0"\ndep = "9.1.2"\n')

    def test_sync_rejects_incorrect_match_count(self):
        self.config["version_files"][0]["count"] = 2
        with self.assertRaisesRegex(ValueError, "expected 2"):
            r.sync_versions(self.config, "0.2.0")

    def test_push_skips_unchanged_and_uses_changed_manifest(self):
        def old(*args):
            return "0.1.7" if args[-1].endswith(":VERSION") else 'version = "0.1.7"\ndep = "9.1.2"'

        with patch.object(r, "git", side_effect=old):
            self.assertIsNone(r.pushed_version(self.config, "a" * 40))
            Path("project.toml").write_text('version = "0.3.0"\ndep = "9.1.2"')
            self.assertEqual(r.pushed_version(self.config, "a" * 40), "0.3.0")
            Path("VERSION").write_text("0.4.0")
            with self.assertRaisesRegex(ValueError, "Conflicting"):
                r.pushed_version(self.config, "a" * 40)

    def test_artifacts_require_every_exact_file(self):
        Path("one.tar.gz").write_bytes(b"one")
        with patch.dict(os.environ, {"RELEASE_NAME": "one"}):
            cfg = {"artifacts": ["${RELEASE_NAME}.tar.gz", "two.zip"]}
            with self.assertRaisesRegex(RuntimeError, "two.zip"):
                r.expected_artifacts(cfg)
            Path("two.zip").write_bytes(b"two")
            self.assertEqual(len(r.expected_artifacts(cfg)), 2)
            Path("two.zip").write_bytes(b"")
            with self.assertRaisesRegex(RuntimeError, "two.zip"):
                r.expected_artifacts(cfg)

    def test_resume_accepts_only_own_immediate_commit(self):
        responses = ["", "b", "own marker", "a", ""]
        with patch.object(r, "git", side_effect=responses):
            self.assertEqual(r.resume_head("main", "a", "own marker"), "b")
        with (
            patch.object(r, "git", side_effect=["", "b", "another run"]),
            self.assertRaisesRegex(RuntimeError, "advanced"),
        ):
            r.resume_head("main", "a", "own marker")
        with (
            patch.object(r, "git", side_effect=["", "b", "own marker", "unrelated"]),
            self.assertRaisesRegex(RuntimeError, "advanced"),
        ):
            r.resume_head("main", "a", "own marker")

    def test_source_archive_uses_proposed_working_bytes(self):
        Path("VERSION").write_text("0.2.0\n")
        env = {
            "RELEASE_NAME": "Demo",
            "RELEASE_VERSION": "0.2.0",
            "RELEASE_DIR": str(Path("out").resolve()),
        }
        with (
            patch.dict(os.environ, env),
            patch.object(p.subprocess, "check_output", return_value="VERSION\0project.toml\0"),
        ):
            target = p.archive("source")
            first = target.read_bytes()
            self.assertEqual(p.archive("source").read_bytes(), first)
            with tarfile.open(target) as tar:
                version_file = tar.extractfile("Demo.v0.2.0/VERSION")
                assert version_file is not None
                self.assertEqual(version_file.read(), Path("VERSION").read_bytes())

    def test_uploaded_bytes_verified_and_retry_preserves_original(self):
        Path("binary.zip").write_bytes(b"local")
        api = r.Forgejo("https://forgejo.invalid", "pub/demo", "placeholder")
        asset = {"id": 1, "name": "binary.zip", "size": 5}
        provenance = {"assets": {}}
        with (
            patch.object(api, "request", side_effect=[[], asset]),
            patch.object(api, "download", return_value=b"wrong"),
            self.assertRaisesRegex(RuntimeError, "digest"),
        ):
            api.upload(1, Path("binary.zip"), provenance, lambda: None)
        provenance["assets"]["binary.zip"] = hashlib.sha256(b"first").hexdigest()
        with (
            patch.object(api, "request", return_value=[asset]) as request,
            patch.object(api, "download", return_value=b"first"),
        ):
            api.upload(1, Path("binary.zip"), provenance, lambda: None)
            self.assertEqual(request.call_count, 1)

    def test_retry_rejects_unproven_or_replaced_same_size_asset(self):
        Path("binary.zip").write_bytes(b"local")
        api = r.Forgejo("https://forgejo.invalid", "pub/demo", "placeholder")
        asset = {"id": 1, "name": "binary.zip", "size": 5}
        with (
            patch.object(api, "request", return_value=[asset]),
            patch.object(api, "download", return_value=b"wrong"),
        ):
            with self.assertRaisesRegex(RuntimeError, "no recorded provenance"):
                api.upload(1, Path("binary.zip"), {"assets": {}}, lambda: None)
            with self.assertRaisesRegex(RuntimeError, "digest mismatch"):
                api.upload(
                    1,
                    Path("binary.zip"),
                    {
                        "assets": {
                            "binary.zip": hashlib.sha256(b"first").hexdigest(),
                        }
                    },
                    lambda: None,
                )

    def test_asset_provenance_is_saved_before_upload(self):
        Path("binary.zip").write_bytes(b"local")
        api = r.Forgejo("https://forgejo.invalid", "pub/demo", "placeholder")
        provenance = {"assets": {}}
        events = []

        def request(method, *_args):
            events.append(method)
            return [] if method == "GET" else {"size": 5}

        def persist():
            self.assertEqual(
                provenance["assets"]["binary.zip"], hashlib.sha256(b"local").hexdigest()
            )
            events.append("saved")

        with (
            patch.object(api, "request", side_effect=request),
            patch.object(api, "download", return_value=b"local"),
        ):
            api.upload(1, Path("binary.zip"), provenance, persist)
        self.assertEqual(events, ["GET", "saved", "POST"])

    def test_release_provenance_is_bound_to_commit_and_read_back(self):
        provenance = {"schema": 1, "tag": "v0.1.7", "commit": "a", "assets": {}, "image": None}
        release = {"body": r.release_body("- change", provenance)}
        self.assertEqual(r.read_provenance(release, "v0.1.7", "a"), provenance)
        with self.assertRaisesRegex(RuntimeError, "does not match"):
            r.read_provenance(release, "v0.1.7", "b")
        api = r.Forgejo("https://forgejo.invalid", "pub/demo", "placeholder")
        with (
            patch.object(api, "request", return_value={"body": "- missing metadata"}),
            self.assertRaisesRegex(RuntimeError, "missing"),
        ):
            r.save_provenance(api, 1, "- change", provenance)

    def test_image_retry_after_push_before_digest_save_reuses_original(self):
        config = "sha256:" + "c" * 64
        digest = "sha256:" + "d" * 64
        provenance = {"image": {"config_digest": config, "digest": None}}
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        with (
            patch.object(
                registry,
                "manifest",
                return_value={
                    "config_digest": config,
                    "digest": digest,
                },
            ),
            patch.object(r, "run") as run,
        ):
            self.assertEqual(
                r.publish_version_image(
                    registry, "forgejo.invalid/pub/demo:v0.1.7", provenance, lambda: None
                ),
                digest,
            )
            run.assert_not_called()
            r.publish_latest_image(registry, "forgejo.invalid/pub/demo", digest)
            self.assertEqual(
                run.call_args_list[0].args, ("podman", "pull", "forgejo.invalid/pub/demo@" + digest)
            )
            self.assertFalse(any(":v0.1.7" in str(call) for call in run.call_args_list))

    def test_image_push_intent_is_saved_before_version_push(self):
        config = "sha256:" + "c" * 64
        provenance = {"image": None}
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        events = []

        def run(*args, **_kwargs):
            if args[1] == "image":
                return config
            events.append("push")
            raise RuntimeError("lost push response")

        def persist():
            self.assertEqual(provenance["image"], {"config_digest": config, "digest": None})
            events.append("saved")

        with (
            patch.object(registry, "manifest", return_value=None),
            patch.object(r, "run", side_effect=run),
            self.assertRaisesRegex(RuntimeError, "lost push"),
        ):
            r.publish_version_image(
                registry, "forgejo.invalid/pub/demo:v0.1.7", provenance, persist
            )
        self.assertEqual(events, ["saved", "push"])

    def test_existing_image_mismatch_is_never_overwritten(self):
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        remote = {"config_digest": "sha256:" + "c" * 64, "digest": "sha256:" + "d" * 64}
        with patch.object(registry, "manifest", return_value=remote), patch.object(r, "run") as run:
            for record in (
                None,
                {"config_digest": "different", "digest": None},
                {"config_digest": remote["config_digest"], "digest": "changed"},
            ):
                with self.assertRaises(RuntimeError):
                    r.publish_version_image(
                        registry, "forgejo.invalid/pub/demo:v0.1.7", {"image": record}, lambda: None
                    )
            run.assert_not_called()

    def test_version_push_client_error_requires_matching_recorded_config(self):
        config = "sha256:" + "c" * 64
        digest = "sha256:" + "d" * 64
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        provenance = {"image": None}
        saved = []

        def persist():
            assert isinstance(provenance["image"], dict)
            saved.append(dict(provenance["image"]))

        with (
            patch.object(
                registry,
                "manifest",
                side_effect=[None, {"config_digest": config, "digest": digest}],
            ),
            patch.object(
                r,
                "run",
                side_effect=[config, subprocess.CalledProcessError(125, ["podman", "push"])],
            ),
        ):
            self.assertEqual(
                r.publish_version_image(
                    registry, "forgejo.invalid/pub/demo:v0.1.7", provenance, persist
                ),
                digest,
            )
        self.assertEqual(
            saved,
            [
                {"config_digest": config, "digest": None},
                {"config_digest": config, "digest": digest},
            ],
        )

    def test_push_client_error_with_missing_wrong_or_unreadable_image_fails(self):
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        for field in ("config_digest", "digest"):
            for observed in (None, {field: "wrong"}, RuntimeError("registry unavailable")):
                with (
                    self.subTest(field=field, observed=observed),
                    patch.object(registry, "manifest", side_effect=[observed]),
                    patch.object(
                        r,
                        "run",
                        side_effect=subprocess.CalledProcessError(125, ["podman", "push"]),
                    ),
                    self.assertRaises(RuntimeError),
                ):
                    r.push_verified_image(
                        registry, "forgejo.invalid/pub/demo:tag", field, "sha256:" + "c" * 64
                    )

    def test_latest_push_client_error_requires_exact_original_manifest(self):
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        digest = "sha256:" + "d" * 64
        with (
            patch.object(registry, "manifest", return_value={"digest": digest}),
            patch.object(
                r,
                "run",
                side_effect=[None, None, subprocess.CalledProcessError(125, ["podman", "push"])],
            ) as run,
        ):
            r.publish_latest_image(registry, "forgejo.invalid/pub/demo", digest)
        self.assertEqual(
            run.call_args_list[0].args, ("podman", "pull", "forgejo.invalid/pub/demo@" + digest)
        )

    def test_latest_pull_failure_is_not_recovered_by_registry_readback(self):
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        with (
            patch.object(registry, "manifest") as manifest,
            patch.object(
                r, "run", side_effect=subprocess.CalledProcessError(125, ["podman", "pull"])
            ),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            r.publish_latest_image(registry, "forgejo.invalid/pub/demo", "sha256:" + "d" * 64)
        manifest.assert_not_called()

    def test_registry_only_404_is_absent(self):
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        for code in (401, 403, 500):
            error = urllib.error.HTTPError(
                "https://forgejo.invalid/v2/pub/demo/manifests/v1",
                code,
                "failed",
                Message(),
                io.BytesIO(),
            )
            with (
                patch.object(r.urllib.request, "urlopen", side_effect=error),
                self.assertRaises(RuntimeError),
            ):
                registry.manifest("forgejo.invalid/pub/demo:v1")
        error = urllib.error.HTTPError(
            "https://forgejo.invalid/v2/pub/demo/manifests/v1",
            404,
            "missing",
            Message(),
            io.BytesIO(),
        )
        with patch.object(r.urllib.request, "urlopen", side_effect=error):
            self.assertIsNone(registry.manifest("forgejo.invalid/pub/demo:v1"))

    def test_registry_bearer_auth_and_manifest_digest(self):
        payload = json.dumps({"config": {"digest": "sha256:" + "c" * 64}}).encode()
        digest = "sha256:" + hashlib.sha256(payload).hexdigest()

        class Response(io.BytesIO):
            def __init__(self, content):
                super().__init__(content)
                self.headers = {"Docker-Content-Digest": digest}

        headers = Message()
        headers["WWW-Authenticate"] = (
            'Bearer realm="https://forgejo.invalid/v2/token",service="forgejo.invalid"'
        )
        challenge = urllib.error.HTTPError(
            "https://forgejo.invalid/v2/", 401, "auth", headers, None
        )
        registry = r.Registry("https://forgejo.invalid", "user", "placeholder")
        with patch.object(
            r.urllib.request,
            "urlopen",
            side_effect=[
                challenge,
                Response(b'{"token":"test-bearer"}'),
                Response(payload),
            ],
        ) as requests:
            self.assertEqual(
                registry.manifest("forgejo.invalid/pub/demo:v1"),
                {
                    "digest": digest,
                    "config_digest": "sha256:" + "c" * 64,
                },
            )
            self.assertTrue(
                requests.call_args_list[1].args[0].get_header("Authorization").startswith("Basic ")
            )
            self.assertEqual(
                requests.call_args_list[2].args[0].get_header("Authorization"), "Bearer test-bearer"
            )
        response = Response(payload)
        response.headers = {"Docker-Content-Digest": "sha256:" + "0" * 64}
        with (
            patch.object(r.urllib.request, "urlopen", return_value=response),
            self.assertRaisesRegex(RuntimeError, "digest mismatch"),
        ):
            registry.manifest("forgejo.invalid/pub/demo:v1")

    def test_binary_architecture_is_checked_before_naming_archive(self):
        elf = bytearray(64)
        elf[:6] = b"\x7fELF\x02\x01"
        elf[18:20] = struct.pack("<H", 183)
        Path("demo").write_bytes(elf)
        p.validate_binary("demo", "linux", "arm64")
        with self.assertRaisesRegex(ValueError, "architecture"):
            p.validate_binary("demo", "linux", "amd64")
        pe = bytearray(128)
        pe[:2] = b"MZ"
        pe[60:64] = struct.pack("<I", 64)
        pe[64:70] = b"PE\0\0" + struct.pack("<H", 0x8664)
        Path("demo.exe").write_bytes(pe)
        p.validate_binary("demo.exe", "windows", "x86_64")
        with self.assertRaisesRegex(ValueError, "architecture"):
            p.validate_binary("demo.exe", "windows", "arm64")

    def run_main(self, fail=None):
        Path(".forgejo").mkdir()
        config = dict(
            self.config,
            checks=["test"],
            build=["build"],
            artifacts=[".release-dist/demo.zip"],
            container={"context": "."},
        )
        Path(".forgejo/release.json").write_text(json.dumps(config))
        event = {
            "repository": {"default_branch": "main", "name": "demo"},
            "inputs": {"bump": "minor"},
        }
        Path("event.json").write_text(json.dumps(event))
        env = {
            "GITHUB_EVENT_PATH": "event.json",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_RUN_ID": "7",
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REPOSITORY": "pub/demo",
            "GITHUB_SERVER_URL": "https://forgejo.invalid",
            "RELEASE_TOKEN": "placeholder",
            "RUNNER_TEMP": self.tmp.name,
        }
        history = []
        state: dict[str, Any] = {"committed": False, "created": False, "published": False}

        def git(*args):
            history.append(("git", *args))
            if args[:1] == ("rev-parse",):
                return "b" if state["committed"] else "a"
            if args[:1] == ("diff",):
                return "VERSION\nproject.toml"
            if args[:1] == ("log",):
                return "change app"
            if args[:1] == ("commit",):
                state["committed"] = True
            return ""

        def commands(items):
            history.append(("commands", *items))
            if items and fail == items[0]:
                raise RuntimeError("simulated failure")
            if items == ["build"]:
                Path(".release-dist/demo.zip").write_bytes(b"binary")

        def request(method, route, data=None, **kwargs):
            history.append(("api", method, route))
            if route == "/tags/v0.1.8":
                sha = "wrong" if fail == "tag" else "b"
                return {"commit": {"sha": sha}} if state["published"] else None
            if route == "/releases?limit=100":
                return []
            if route == "/releases/tags/v0.1.8":
                return None
            if method == "POST":
                assert data is not None
                self.assertIn(
                    "[Container image](https://forgejo.invalid/pub/-/packages/container/demo/v0.1.8)",
                    data["body"],
                )
                state["created"] = True
                state["release"] = {"id": 1, **data}
                return state["release"]
            if route.endswith("/assets"):
                return [{"name": "demo.zip"}]
            if method == "PATCH":
                assert data is not None
                if fail == "publish" and data.get("draft") is False:
                    raise RuntimeError("simulated publication failure")
                state["release"].update(data)
                if data.get("draft") is False:
                    state["published"] = True
            if route == "/releases/1":
                return state["release"]
            return {}

        def run(*args, **kwargs):
            history.append(("run", *args))
            if args[1:3] == ("image", "inspect"):
                return "sha256:" + "c" * 64

        with (
            patch.dict(os.environ, env),
            patch.object(r, "git", side_effect=git),
            patch.object(r, "resume_head", return_value="a"),
            patch.object(r, "assert_remote_head"),
            patch.object(r, "commands", side_effect=commands),
            patch.object(r, "run", side_effect=run),
            patch.object(r.Forgejo, "request", side_effect=request),
            patch.object(r.Forgejo, "upload", side_effect=lambda *a: history.append(("upload",))),
            patch.object(
                r.Registry,
                "manifest",
                side_effect=[
                    None,
                    {
                        "config_digest": "sha256:" + "c" * 64,
                        "digest": "sha256:" + "d" * 64,
                    },
                    {"config_digest": "sha256:" + "c" * 64, "digest": "sha256:" + "d" * 64},
                ],
            ),
        ):
            if fail:
                with self.assertRaisesRegex(
                    RuntimeError, "Published release tag" if fail == "tag" else "simulated"
                ):
                    r.main()
            else:
                r.main()
        return history

    def test_checks_fail_before_commit_or_publish(self):
        history = self.run_main("test")
        self.assertFalse(any(h[:2] == ("git", "commit") or h[0] == "api" for h in history))

    def test_build_fail_before_commit_or_publish(self):
        history = self.run_main("build")
        self.assertFalse(any(h[:2] == ("git", "commit") or h[0] == "api" for h in history))

    def test_publication_order(self):
        history = self.run_main()

        def position(prefix):
            return next(i for i, h in enumerate(history) if h[: len(prefix)] == prefix)

        self.assertLess(position(("commands", "build")), position(("git", "commit")))
        self.assertLess(position(("run", "podman", "build")), position(("git", "commit")))
        self.assertLess(position(("upload",)), position(("run", "podman", "push")))
        self.assertLess(position(("api", "PATCH")), position(("run", "podman", "tag")))

    def test_publication_failure_keeps_version_and_never_updates_latest(self):
        history = self.run_main("publish")
        self.assertEqual(Path("VERSION").read_text().strip(), "0.1.8")
        self.assertTrue(any(h[:2] == ("git", "commit") for h in history))
        self.assertFalse(any(h[:3] == ("run", "podman", "tag") for h in history))

    def test_draft_without_tag_is_verified_after_publication_before_latest(self):
        history = self.run_main()
        tag_reads = [i for i, h in enumerate(history) if h == ("api", "GET", "/tags/v0.1.8")]
        latest = next(i for i, h in enumerate(history) if h[:3] == ("run", "podman", "tag"))
        version_push = next(i for i, h in enumerate(history) if h[:3] == ("run", "podman", "push"))
        self.assertEqual(len(tag_reads), 3)
        self.assertLess(version_push, tag_reads[-1])
        self.assertLess(tag_reads[-1], latest)

    def test_wrong_published_tag_prevents_latest(self):
        history = self.run_main("tag")
        self.assertFalse(any(h[:3] == ("run", "podman", "tag") for h in history))


if __name__ == "__main__":
    unittest.main()
