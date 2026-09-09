#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Offline publication tests, executed only by the Docker test-publish target."""

import hashlib
import io
import json
from contextlib import nullcontext
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch
import zipfile

import app_version
import publish

MANIFEST = '[package]\nname = "stillus-app"\nversion = "0.1.0"\n\n[dependencies]\nother = "0.1.0"\n'
LOCK = ('version = 4\n\n[[package]]\nname = "stillus-app"\nversion = "0.1.0"\n'
        '\n[[package]]\nname = "other"\nversion = "0.1.0"\n')
SHA = "a" * 40


def fixture_archive(path, *, target="linux", sha=SHA, corrupt=False, signed=True):
    files = {"stillus": b"application", "SOURCE_REVISION.txt": (sha + "\n").encode()}
    if target == "macos":
        files["Stillus.app/Contents/Resources/release.json"] = json.dumps({
            "source_revision": sha, "version": "0.1.1"}).encode()
        if signed:
            files["Stillus.app/Contents/_CodeSignature/CodeResources"] = b"adhoc signature"
    records = [{"path": name, "sha256": hashlib.sha256(data).hexdigest()} for name, data in files.items()]
    files["build.json"] = json.dumps({"source_revision": sha, "platform": target,
                                      "architecture": "arm64", "files": records}).encode()
    if corrupt:
        files["stillus"] = b"changed after packaging"
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, "w") as archive:
            for name, data in files.items():
                archive.writestr(name, data)
    else:
        with tarfile.open(path, "w:gz") as archive:
            for name, data in files.items():
                info = tarfile.TarInfo("stillus-linux-arm64/" + name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))


class VersionTests(unittest.TestCase):
    def test_git_runs_on_host_with_authentication_only_in_environment(self):
        github = publish.GitHub("stillus-ai/stillus", "secret")
        with patch.object(publish, "run", return_value="") as execute:
            github.network_git("ls-remote", github.url, "refs/tags/latest")
        command = execute.call_args.args[0]
        self.assertEqual(command, ["git", "ls-remote", github.url, "refs/tags/latest"])
        self.assertNotIn("secret", " ".join(command))
        self.assertIn("GIT_CONFIG_VALUE_0", execute.call_args.kwargs["env"])

    def test_remote_tag_distinguishes_raw_object_from_peeled_commit(self):
        github = publish.GitHub("stillus-ai/stillus", "secret")
        raw = "b" * 40
        refs = f"{raw}\trefs/tags/latest\n{SHA}\trefs/tags/latest^{{}}\n"
        with patch.object(github, "network_git", return_value=refs):
            self.assertEqual(github.remote_tag("latest"), SHA)
            self.assertEqual(github.remote_tag("latest", peel=False), raw)
        with patch.object(github, "network_git", return_value=""):
            self.assertIsNone(github.remote_tag("latest", peel=False))

    def test_increments_only_patch(self):
        for old, new in (("0.1.0", "0.1.1"), ("0.1.9", "0.1.10"), ("2.10.99", "2.10.100")):
            self.assertEqual(app_version.next_version(old), new)
        for invalid in ("0.1", "0.1.0-rc1", "v0.1.0", "01.1.0", "0.1.65535"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                app_version.next_version(invalid)

    def test_changes_application_only(self):
        updated = app_version.replace_version(MANIFEST, "0.1.0", "0.1.1")
        self.assertEqual(app_version.read_version(updated), "0.1.1")
        self.assertIn('other = "0.1.0"', updated)
        lock = app_version.replace_version(LOCK, "0.1.0", "0.1.1", lock=True)
        self.assertIn('name = "stillus-app"\nversion = "0.1.1"', lock)
        self.assertIn('name = "other"\nversion = "0.1.0"', lock)
        with self.assertRaises(ValueError):
            app_version.replace_version(LOCK, "0.2.0", "0.2.1", lock=True)

    def test_repository_resolution(self):
        for url in ("git@github.com:stillus-ai/stillus.git", "https://github.com/stillus-ai/stillus",
                    "ssh://git@github.com/stillus-ai/stillus.git"):
            self.assertEqual(publish.repository_from_remote(url), "stillus-ai/stillus")
        for url in ("https://secret@github.com/org/repo", "https://example.com/org/repo", "../repo"):
            with self.assertRaises(ValueError):
                publish.repository_from_remote(url)

    def test_codex_uses_requested_model_and_structured_output(self):
        def execute(command, **kwargs):
            self.assertEqual(command[0], "/example/codex")
            self.assertEqual(command[command.index("--model") + 1], "gpt-5.6-luna")
            self.assertIn('model_reasoning_effort="medium"', command)
            self.assertEqual(command[command.index("--sandbox") + 1], "read-only")
            self.assertEqual(command[command.index("--cd") + 1], str(publish.ROOT))
            self.assertIn("Git range", kwargs["input"])
            self.assertIn("b" * 40 + ".." + SHA, kwargs["input"])
            self.assertEqual(kwargs["stdout"], subprocess.DEVNULL)
            self.assertEqual(kwargs["stderr"], subprocess.PIPE)
            output = Path(command[command.index("--output-last-message") + 1])
            output.write_text('{"improvements":["Faster search."],"bug_fixes":[]}', encoding="utf-8")
        with patch.object(publish.shutil, "which", return_value="/example/codex"), \
                patch.object(publish, "run", side_effect=execute):
            self.assertEqual(publish.codex_notes("b" * 40, SHA)["improvements"], ["Faster search."])
        with patch.object(publish.shutil, "which", return_value="/example/codex"), \
                patch.object(publish, "run", side_effect=subprocess.CalledProcessError(1, ["codex"])):
            with self.assertRaisesRegex(ValueError, "exit status 1"):
                publish.codex_notes(None, SHA)
        for value in ({}, {"improvements": [""], "bug_fixes": []},
                      {"improvements": "bad", "bug_fixes": []}):
            with self.assertRaises(ValueError):
                publish.validate_notes(value)


class RepositoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "app/stillus").mkdir(parents=True)
        (self.root / app_version.MANIFEST).write_text(MANIFEST, encoding="utf-8")
        (self.root / "Cargo.lock").write_text(LOCK, encoding="utf-8")
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.git("add", ".")
        self.git("commit", "-qm", "Initial version")
        self.initial = self.git("rev-parse", "HEAD").strip()
        self.state_path = self.root / ".git/publish.json"
        for name, value in (("ROOT", self.root), ("STATE", self.state_path), ("git", self.git)):
            patcher = patch.object(publish, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def git(self, *args, input=None, env=None, stdout=subprocess.PIPE):
        return subprocess.run(["git", *args], cwd=self.root, input=input, text=True,
                              env={**publish.os.environ, **(env or {})}, stdout=stdout,
                              stderr=subprocess.PIPE, check=True).stdout or ""

    def feature(self):
        (self.root / "feature.txt").write_text("feature", encoding="utf-8")
        self.git("add", "feature.txt")
        self.git("commit", "-qm", "Add a feature")
        return self.git("rev-parse", "HEAD").strip()

    def state(self):
        with patch.object(publish, "release_notes", return_value="## Improvements\n\n- Feature.\n"):
            return publish.new_state("stillus-ai/stillus", "master", self.feature(),
                                     released_current_sha=self.initial)

    def test_boundary_uses_version_value_not_any_manifest_edit(self):
        head = self.feature()
        self.assertIsNone(publish.previous_version_commit(head))
        manifest = self.root / app_version.MANIFEST
        manifest.write_text(MANIFEST.replace("[dependencies]", "# Formatting\n[dependencies]"), encoding="utf-8")
        self.git("add", app_version.MANIFEST)
        self.git("commit", "-qm", "Document dependencies")
        self.assertIsNone(publish.previous_version_commit("HEAD"))
        manifest.write_text(MANIFEST.replace('version = "0.1.0"', 'version = "0.1.1"'), encoding="utf-8")
        self.git("add", app_version.MANIFEST)
        self.git("commit", "-qm", "Release version")
        bumped = self.git("rev-parse", "HEAD").strip()
        self.assertEqual(publish.previous_version_commit("HEAD"), bumped)
        with self.assertRaisesRegex(ValueError, "no commits"):
            publish.new_state("stillus-ai/stillus", "master", bumped)

    def test_initial_release_keeps_current_version_and_head(self):
        head = self.feature()
        with patch.object(publish, "release_notes", return_value="Initial notes") as notes:
            state = publish.new_state("stillus-ai/stillus", "master", head)
        self.assertEqual(state["version"], "0.1.0")
        self.assertEqual(state["tag"], "v0.1.0")
        self.assertEqual(state["sha"], head)
        self.assertEqual(state["updates"], {})
        notes.assert_called_once_with(None, head)
        publish.commit_version(state)
        self.assertEqual(self.git("rev-list", "--count", f"{head}..HEAD").strip(), "0")

    def test_release_after_initial_tag_increments_patch(self):
        head = self.feature()
        with patch.object(publish, "release_notes", return_value="Next notes") as notes:
            state = publish.new_state("stillus-ai/stillus", "master", head,
                                      released_current_sha=self.initial)
        self.assertEqual(state["version"], "0.1.1")
        self.assertIsNone(state["sha"])
        notes.assert_called_once_with(self.initial, head)

    def test_commit_and_crash_recovery_do_not_bump_twice(self):
        state = self.state()
        publish.save_state(state)
        publish.commit_version(state)
        release = state["sha"]
        paths = set(self.git("diff", "--name-only", state["head"], release).splitlines())
        self.assertEqual(paths, set(app_version.VERSION_FILES))
        self.assertEqual(self.git("log", "-1", "--format=%ae").strip(), publish.IDENTITY["GIT_AUTHOR_EMAIL"])
        state["sha"] = None  # Simulate a crash just after Git committed, before saving the SHA.
        publish.commit_version(state)
        self.assertEqual(state["sha"], release)
        self.assertEqual(app_version.read_version((self.root / app_version.MANIFEST).read_text()), "0.1.1")
        self.assertEqual(self.git("rev-list", "--count", f"{state['head']}..HEAD").strip(), "1")

    def test_unrelated_changes_are_preserved(self):
        state = self.state()
        path = self.root / "unrelated.txt"
        path.write_text("another agent", encoding="utf-8")
        self.git("add", "unrelated.txt")
        with self.assertRaisesRegex(ValueError, "unrelated"):
            publish.commit_version(state)
        self.assertEqual(path.read_text(), "another agent")
        self.assertEqual((self.root / app_version.MANIFEST).read_text(), MANIFEST)
        self.assertEqual(self.git("diff", "--cached", "--name-only").strip(), "unrelated.txt")

    def test_partial_version_write_can_be_resumed(self):
        state = self.state()
        (self.root / app_version.MANIFEST).write_text(state["updates"][app_version.MANIFEST], encoding="utf-8")
        publish.commit_version(state)
        self.assertEqual((self.root / "Cargo.lock").read_text(), state["updates"]["Cargo.lock"])

    def test_independent_version_edit_is_rejected(self):
        state = self.state()
        manifest = self.root / app_version.MANIFEST
        manifest.write_text(MANIFEST + "# another agent\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "independently"):
            publish.commit_version(state)
        self.assertTrue(manifest.read_text().endswith("# another agent\n"))


class ArchiveTests(unittest.TestCase):
    def test_provenance_and_every_file_checksum(self):
        with tempfile.TemporaryDirectory() as temporary:
            for extension in ("tar.gz", "zip"):
                path = Path(temporary) / ("fixture." + extension)
                fixture_archive(path)
                self.assertEqual(publish.validate_archive(path, SHA, "0.1.1", "linux"), "arm64")
                with self.assertRaisesRegex(ValueError, "different build"):
                    publish.validate_archive(path, "b" * 40, "0.1.1", "linux")
                fixture_archive(path, corrupt=True)
                with self.assertRaisesRegex(ValueError, "checksum"):
                    publish.validate_archive(path, SHA, "0.1.1", "linux")

    def test_macos_version_is_verified(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "fixture.tar.gz"
            fixture_archive(path, target="macos")
            self.assertEqual(publish.validate_archive(path, SHA, "0.1.1", "macos"), "arm64")
            with self.assertRaisesRegex(ValueError, "version"):
                publish.validate_archive(path, SHA, "0.1.2", "macos")

    def test_macos_archive_requires_bundle_signature(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "fixture.tar.gz"
            fixture_archive(path, target="macos", signed=False)
            with self.assertRaisesRegex(ValueError, "no bundle code signature"):
                publish.validate_archive(path, SHA, "0.1.1", "macos")

    def test_failed_build_does_not_mark_assets_ready(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = {"version": "0.1.1", "sha": SHA, "assets": {}}
            with patch.object(publish, "WORK", Path(temporary)), patch.object(publish, "require_clean"), \
                    patch.object(publish, "run", side_effect=subprocess.CalledProcessError(2, ["make"])):
                with self.assertRaises(subprocess.CalledProcessError):
                    publish.build_assets(state)
            self.assertEqual(state["assets"], {})

    def test_ready_assets_are_not_rebuilt_and_changed_bytes_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "0.1.1").mkdir()
            path = root / "0.1.1/archive.zip"
            path.write_bytes(b"archive")
            state = {"version": "0.1.1", "sha": SHA, "assets": {path.name: publish.file_digest(path)}}
            with patch.object(publish, "WORK", root), patch.object(publish, "run") as execute:
                publish.build_assets(state)
                execute.assert_not_called()
                path.write_bytes(b"changed")
                with self.assertRaisesRegex(ValueError, "assets changed"):
                    publish.build_assets(state)


class UploadTests(unittest.TestCase):
    def test_draft_retry_skips_existing_assets_and_verifies_digests(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            asset = directory / "stillus.zip"
            asset.write_bytes(b"application")
            state = {"sha": SHA, "tag": "v0.1.1", "branch": "master", "notes": "Notes",
                     "assets": {asset.name: publish.file_digest(asset)}, "published": False}
            release = {"id": 42, "body": "Notes", "draft": True,
                       "html_url": "https://example.invalid/release"}
            github = Mock()
            github.url = "https://github.com/stillus-ai/stillus.git"
            github.remote_tag.return_value = SHA
            github.release.return_value = release
            github.json_request.return_value = {"tag_name": state["tag"], "draft": False,
                                                "prerelease": False}
            remote_asset = {"name": asset.name, "state": "uploaded", "size": asset.stat().st_size,
                            "digest": "sha256:" + publish.file_digest(asset)}
            github.api.return_value = [[remote_asset]]
            github.publish_release.return_value = {**release, "draft": False}

            def git(*args, **kwargs):
                if args[0] == "tag":
                    return "v0.1.1\n"
                return SHA + "\n"

            with patch.object(publish, "git", side_effect=git), patch.object(publish, "require_clean"), \
                    patch.object(publish, "save_state"):
                publish.upload_release(state, github, directory)
            self.assertTrue(state["published"])
            github.upload_asset.assert_not_called()
            github.verify_asset.assert_called_once_with(remote_asset, asset)
            github.publish_release.assert_called_once_with(release)

    def test_missing_asset_is_uploaded_through_api(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            asset = directory / "stillus.zip"
            asset.write_bytes(b"application")
            state = {"sha": SHA, "tag": "v0.1.1", "branch": "master", "notes": "Notes",
                     "assets": {asset.name: publish.file_digest(asset)}, "published": False}
            release = {"id": 42, "body": "Notes", "draft": True,
                       "html_url": "https://example.invalid/release"}
            uploaded = {"name": asset.name, "state": "uploaded", "size": asset.stat().st_size,
                        "digest": "sha256:" + publish.file_digest(asset)}
            github = Mock()
            github.url = "https://github.com/stillus-ai/stillus.git"
            github.remote_tag.return_value = SHA
            github.release.return_value = release
            github.json_request.return_value = {"tag_name": state["tag"], "draft": False,
                                                "prerelease": False}
            github.api.side_effect = [[[]], [[uploaded]]]
            github.upload_asset.return_value = uploaded
            github.publish_release.return_value = {**release, "draft": False}

            def git(*args, **kwargs):
                if args[0] == "tag":
                    return "v0.1.1\n"
                return SHA + "\n"

            with patch.object(publish, "git", side_effect=git), patch.object(publish, "require_clean"), \
                    patch.object(publish, "save_state"):
                publish.upload_release(state, github, directory)
            github.upload_asset.assert_called_once_with(release, asset)
            github.verify_asset.assert_called_once_with(uploaded, asset)

    def test_asset_digest_must_match_local_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            asset = Path(temporary) / "stillus.zip"
            asset.write_bytes(b"application")
            github = publish.GitHub("stillus-ai/stillus", "secret")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                github.verify_asset({"state": "uploaded", "size": asset.stat().st_size,
                                     "digest": "sha256:" + "0" * 64}, asset)
            with self.assertRaisesRegex(ValueError, "did not return"):
                github.verify_asset({"state": "uploaded", "size": asset.stat().st_size}, asset)
            with self.assertRaisesRegex(ValueError, "unexpected size"):
                github.verify_asset({"state": "uploaded", "size": asset.stat().st_size + 1,
                                     "digest": "sha256:" + publish.file_digest(asset)}, asset)

    def test_conflicting_remote_tag_is_never_overwritten(self):
        github = Mock()
        github.remote_tag.return_value = "b" * 40
        with patch.object(publish, "require_clean"), self.assertRaisesRegex(ValueError, "different commit"):
            publish.upload_release({"sha": SHA, "tag": "v0.1.1"}, github, Path("/unused"))
        github.network_git.assert_not_called()

    def test_asset_failure_never_publishes_release_or_moves_latest(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            asset = directory / "stillus.zip"
            asset.write_bytes(b"application")
            state = {"sha": SHA, "tag": "v0.1.1", "branch": "master", "notes": "Notes",
                     "assets": {asset.name: publish.file_digest(asset)}, "published": False}
            github = Mock()
            github.remote_tag.return_value = SHA
            github.release.return_value = {"id": 42, "body": "Notes", "draft": True}
            github.api.return_value = [[{"name": asset.name}]]
            github.verify_asset.side_effect = ValueError("checksum mismatch")
            with patch.object(publish, "git", return_value=SHA), \
                    patch.object(publish, "require_clean"), patch.object(publish, "save_state"), \
                    patch.object(publish, "finish_publication") as finish:
                with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                    publish.upload_release(state, github, directory)
            github.publish_release.assert_not_called()
            finish.assert_not_called()
            self.assertFalse(state["published"])
            self.assertNotIn("release_published", state)
            self.assertFalse(any("refs/tags/latest" in str(call)
                                 for call in github.network_git.call_args_list))


class LatestTests(unittest.TestCase):
    def setUp(self):
        self.state = {"sha": SHA, "tag": "v0.1.1", "published": False,
                      "release_published": True, "url": "https://example.invalid/release"}
        self.remote = None
        self.github = Mock()
        self.github.url = "https://github.com/stillus-ai/stillus.git"
        self.github.api_url = "https://api.github.com/repos/stillus-ai/stillus"
        self.github.json_request.return_value = {
            "tag_name": "v0.1.1", "draft": False, "prerelease": False}
        self.github.remote_tag.side_effect = (
            lambda tag, **kwargs: self.remote if tag == "latest" else SHA)
        self.github.network_git.side_effect = self.push
        for name in ("require_clean", "save_state", "git"):
            patcher = patch.object(publish, name)
            setattr(self, name, patcher.start())
            self.addCleanup(patcher.stop)
        self.git.return_value = ""
        self.saved = []
        self.save_state.side_effect = lambda state: self.saved.append(dict(state))

    def push(self, *args):
        self.assertEqual(args, ("push",
                               f"--force-with-lease=refs/tags/latest:{self.remote or ''}",
                               self.github.url, f"{SHA}:refs/tags/latest"))
        self.remote = SHA
        return ""

    def test_first_publication_creates_lightweight_latest(self):
        publish.finish_publication(self.state, self.github)
        self.assertEqual(self.remote, SHA)
        self.assertIsNone(self.saved[0]["latest_previous"])
        self.assertFalse(self.saved[0]["published"])
        self.git.assert_any_call("update-ref", "--no-deref", "refs/tags/latest", SHA, "0" * 40)
        self.assertTrue(self.saved[-1]["published"])

    def test_moving_latest_preserves_version_tags_and_uses_raw_lease(self):
        self.remote = "b" * 40  # Raw object ID of an annotated predecessor tag.
        self.git.return_value = "c" * 40
        publish.finish_publication(self.state, self.github)
        self.assertEqual(self.state["latest_previous"], "b" * 40)
        self.github.network_git.assert_called_once()
        self.git.assert_any_call("update-ref", "--no-deref", "refs/tags/latest", SHA, "c" * 40)
        self.assertFalse(any("refs/tags/v0.1.1" in str(call) for call in self.git.call_args_list))

    def test_accepted_push_with_lost_response_can_be_resumed(self):
        def interrupted_push(*args):
            self.push(*args)
            raise subprocess.CalledProcessError(1, ["git", "push"])
        self.github.network_git.side_effect = interrupted_push
        with self.assertRaises(subprocess.CalledProcessError):
            publish.finish_publication(self.state, self.github)
        self.assertFalse(self.state["published"])
        self.git.assert_not_called()
        publish.finish_publication(self.state, self.github)
        self.github.network_git.assert_called_once()
        self.assertTrue(self.state["published"])

    def test_local_update_failure_retries_without_another_push(self):
        def local_git(*args):
            if args[0] == "update-ref":
                raise subprocess.CalledProcessError(1, ["git", "update-ref"])
            return ""
        self.git.side_effect = local_git
        with self.assertRaises(subprocess.CalledProcessError):
            publish.finish_publication(self.state, self.github)
        self.assertFalse(self.state["published"])
        self.git.side_effect = None
        publish.finish_publication(self.state, self.github)
        self.github.network_git.assert_called_once()
        self.assertTrue(self.state["published"])

    def test_concurrent_remote_change_is_not_overwritten(self):
        publish.prepare_latest(self.state, self.github)
        self.remote = "b" * 40
        with self.assertRaisesRegex(ValueError, "remote latest changed"):
            publish.finish_publication(self.state, self.github)
        self.github.network_git.assert_not_called()
        self.git.assert_not_called()
        self.assertIsNone(self.state["latest_previous"])

    def test_change_between_read_and_push_keeps_original_lease(self):
        self.state["latest_previous"] = "b" * 40
        self.remote = "b" * 40
        self.github.network_git.side_effect = subprocess.CalledProcessError(1, ["git", "push"])
        with self.assertRaises(subprocess.CalledProcessError):
            publish.finish_publication(self.state, self.github)
        self.assertIn("--force-with-lease=refs/tags/latest:" + "b" * 40,
                      self.github.network_git.call_args.args)
        self.git.assert_not_called()
        self.assertFalse(self.state["published"])

    def test_legacy_completed_state_can_repair_latest(self):
        self.state.pop("release_published")
        self.state["published"] = True
        publish.finish_publication(self.state, self.github)
        self.assertEqual(self.remote, SHA)
        self.assertTrue(self.state["published"])

    def test_main_resumes_tag_update_without_rebuilding_or_bumping(self):
        self.state.update(format=1, repository="stillus-ai/stillus", branch="master")
        self.github.json_request.side_effect = lambda method, url: (
            {"tag_name": "v0.1.1", "draft": False, "prerelease": False}
            if url.endswith("/releases/latest") else {"default_branch": "master"})
        self.git.side_effect = lambda *args: (
            "git@github.com:stillus-ai/stillus.git" if args[0] == "remote" else
            "master" if args[0] == "symbolic-ref" else SHA)
        saved_file = Mock()
        saved_file.exists.return_value = True
        saved_file.read_text.return_value = json.dumps(self.state)
        with patch.object(publish, "STATE", saved_file), \
                patch.object(publish.platform, "system", return_value="Darwin"), \
                patch.object(publish.platform, "machine", return_value="arm64"), \
                patch.object(publish.shutil, "which", return_value="/fixture/tool"), \
                patch.dict(publish.os.environ, {"GITHUB_TOKEN": "fixture-token"}), \
                patch.object(publish, "publish_lock", return_value=nullcontext()), \
                patch.object(publish, "GitHub", return_value=self.github), \
                patch.object(publish, "new_state") as new_state, \
                patch.object(publish, "build_assets") as build, \
                patch.object(publish, "upload_release") as upload:
            publish.main()
        self.assertEqual(self.remote, SHA)
        self.assertTrue(self.saved[-1]["published"])
        new_state.assert_not_called()
        build.assert_not_called()
        upload.assert_not_called()

    def test_older_release_cannot_move_latest_backwards(self):
        self.state["published"] = True
        self.github.json_request.return_value["tag_name"] = "v0.1.2"
        with self.assertRaisesRegex(ValueError, "no longer GitHub Latest"):
            publish.finish_publication(self.state, self.github)
        self.github.network_git.assert_not_called()
        self.git.assert_not_called()

    def test_changed_release_tag_prevents_latest_update(self):
        self.github.remote_tag.side_effect = None
        self.github.remote_tag.return_value = "b" * 40
        with self.assertRaisesRegex(ValueError, "remote release tag changed"):
            publish.finish_publication(self.state, self.github)
        self.github.network_git.assert_not_called()

    def test_workflow_runs_for_the_moving_latest_tag(self):
        workflow = (app_version.ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("    branches: [master]\n    tags: [latest]", workflow)


if __name__ == "__main__":
    unittest.main()
