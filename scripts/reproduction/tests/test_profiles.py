# SPDX-License-Identifier: MIT OR Apache-2.0
"""Subject isolation and fixed-workflow boundaries; no hosted state is spoofed."""
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_current_pin as build
import verify_current_pin as verify
from profiles import ANDROID_PAGE_SIZE, HISTORICAL, Manifest, load_manifest, select_profile


class ProfileTests(unittest.TestCase):
    def test_historical_manifest_workflow_and_default_receipt_preserved(self):
        manifest = load_manifest()
        self.assertEqual(manifest.profile, HISTORICAL)
        self.assertEqual(manifest.sha256, "8a21907438c9aaa3c25e5e9186c36bf45519b87abe2f428623129c488c33b937")
        workflow = Path(__file__).resolve().parents[3] / ".github/workflows/clean-reproduction.yml"
        self.assertEqual(hashlib.sha256(workflow.read_bytes()).hexdigest(), "b588d1b96806cad6408bde07f75339f66e5825b8977a9c4e3812509315bc3d7e")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worker = build.Builder(root, root / "evidence", {})
            self.assertEqual(worker.receipt["source_commit"], HISTORICAL.source_commit)
            self.assertNotIn("reconstruction_profile", worker.receipt)
            self.assertNotIn("expected_manifest_sha256", worker.receipt)

    def test_candidate_builder_binds_new_source_and_raw_manifest(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worker = build.Builder(root, root / "evidence", {}, ANDROID_PAGE_SIZE)
            self.assertEqual(worker.receipt["source_commit"], ANDROID_PAGE_SIZE.source_commit)
            self.assertEqual(worker.receipt["source_tree"], ANDROID_PAGE_SIZE.source_tree)
            self.assertEqual(worker.receipt["expected_manifest_sha256"], manifest.sha256)
            self.assertEqual(worker.receipt["reconstruction_profile"], ANDROID_PAGE_SIZE.name)

    def test_cross_profile_unknown_profile_and_modified_subject_rejected(self):
        with self.assertRaises(ValueError):
            Manifest(ANDROID_PAGE_SIZE, load_manifest().raw)
        with self.assertRaises(ValueError):
            select_profile("../../expected.json")
        with self.assertRaises(dataclasses.FrozenInstanceError):
            ANDROID_PAGE_SIZE.source_commit = "0" * 40
        for field, value in (("commit", "0" * 40), ("tree", "0" * 40),
                             ("repository", "https://example.invalid/unreviewed.git")):
            data = load_manifest(ANDROID_PAGE_SIZE).document
            data["source"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                Manifest(ANDROID_PAGE_SIZE, json.dumps(data).encode())

    def test_manifest_duplicate_keys_wrong_xcode_and_policy_rejected(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        with self.assertRaises(ValueError):
            Manifest(ANDROID_PAGE_SIZE, b'{"source":null,' + manifest.raw.lstrip()[1:])
        for field in ("xcode", "flags", "profile"):
            data = manifest.document
            if field == "xcode":
                data["toolchain"]["xcode_identity"] = "Xcode 26.6\nBuild version 17F42"
            elif field == "flags":
                data["android_link_args"] = []
            else:
                data["reconstruction_profile"] = HISTORICAL.name
            with self.subTest(field=field), self.assertRaises(ValueError):
                Manifest(ANDROID_PAGE_SIZE, json.dumps(data).encode())

    def test_manifest_schema_and_complete_artifact_set_required(self):
        for field in ("schema", "missing", "duplicate", "size", "digest"):
            data = load_manifest(ANDROID_PAGE_SIZE).document
            if field == "schema":
                data["schema"] = True
            elif field == "missing":
                data["artifacts"].pop()
            elif field == "duplicate":
                data["artifacts"][-1] = data["artifacts"][0]
            elif field == "size":
                data["artifacts"][0]["size_bytes"] = 0
            else:
                data["artifacts"][0]["sha256"] = "unbound"
            with self.subTest(field=field), self.assertRaises(ValueError):
                Manifest(ANDROID_PAGE_SIZE, json.dumps(data).encode())

    def test_parsed_manifest_cannot_mutate_bound_bytes_or_early_failure_labels(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        altered = manifest.document
        altered["source"]["commit"] = "0" * 40
        self.assertEqual(manifest.document["source"]["commit"], ANDROID_PAGE_SIZE.source_commit)
        with patch.object(verify, "validate_environment", side_effect=verify.VerificationError("synthetic-environment-failure")):
            result = verify.verify(Path("/source"), Path("/artifacts"), Path("/ndk"), manifest.document, manifest=manifest)
        self.assertEqual(result["expected_manifest_sha256"], hashlib.sha256(manifest.raw).hexdigest())
        self.assertEqual(result["evidence_kind"], "android-page-size-candidate-verification")
        self.assertEqual(result["source_commit"], ANDROID_PAGE_SIZE.source_commit)
        self.assertFalse(result["fresh_build_proven"])
        with patch.object(verify, "validate_environment") as environment:
            result = verify.verify(Path("/source"), Path("/artifacts"), Path("/ndk"), altered, manifest=manifest)
        environment.assert_not_called()
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["source_commit"], ANDROID_PAGE_SIZE.source_commit)

    def test_candidate_wrong_source_rejected_before_toolchain(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        with patch.object(verify, "validate_environment", return_value=(Path("/home"), Path("/cargo"))), patch.object(verify, "check_source", side_effect=verify.VerificationError("source-commit-mismatch")), patch.object(verify, "check_toolchain") as tools:
            result = verify.verify(Path("/source"), Path("/artifacts"), Path("/ndk"), manifest.document, manifest=manifest)
        self.assertEqual(result["status"], "fail")
        tools.assert_not_called()

    def test_candidate_requires_exact_xcode_build_not_only_major_version(self):
        expected = load_manifest(ANDROID_PAGE_SIZE).document["toolchain"]
        expected["ndk_files"] = []
        with tempfile.TemporaryDirectory() as directory:
            ndk = Path(directory)
            (ndk / "source.properties").write_text("Pkg.Revision = 27.2.12479018\n")
            for identity in (ANDROID_PAGE_SIZE.xcode_identity, "Xcode 27.0\nBuild version 27A000", "Xcode 26.6\nBuild version 17F42"):
                def run(argv):
                    if argv == ["rustc", "-vV"]:
                        return "host: aarch64-apple-darwin\n"
                    for key, command in (("rustc_version", ["rustc", "--version"]), ("cargo_version", ["cargo", "--version"]), ("cargo_ndk_version", ["cargo", "ndk", "--version"])):
                        if argv == command:
                            return expected[key]
                    return identity if argv[0].endswith("xcodebuild") else "27.0"
                with self.subTest(identity=identity), patch.object(verify.sys, "platform", "darwin"), patch.object(verify.platform, "machine", return_value="arm64"), patch.object(verify, "run", side_effect=run):
                    if identity == ANDROID_PAGE_SIZE.xcode_identity:
                        verify.check_toolchain(ndk, expected, ANDROID_PAGE_SIZE)
                    else:
                        with self.assertRaisesRegex(verify.VerificationError, "candidate-xcode"):
                            verify.check_toolchain(ndk, expected, ANDROID_PAGE_SIZE)

    def test_versioned_xcode_alias_resolves_but_unreviewed_selection_rejected(self):
        alias = ANDROID_PAGE_SIZE.developer_directory
        resolved = Path("/Applications/Xcode_27_Release_Candidate.app/Contents/Developer")
        environment = {"HOME": "/home", "RUNNER_TEMP": "/tmp/runner", "DEVELOPER_DIR": alias,
                       "ANDROID_SDK_ROOT": "/sdk", "TMPDIR": "/tmp"}
        def resolve(path):
            return resolved if str(path) == alias else path
        with patch.dict(os.environ, environment), patch.object(verify.shutil, "which", return_value="/tmp/runner/sondefox-reproduction-tools/bin/cargo-ndk"), patch.object(Path, "resolve", autospec=True, side_effect=resolve):
            self.assertEqual(verify.build_path_roles(Path("/source"), Path("/ndk"), ANDROID_PAGE_SIZE)["$DEVELOPER_DIR"], resolved)
            with patch.dict(os.environ, {"DEVELOPER_DIR": "/Applications/Xcode.app/Contents/Developer"}), self.assertRaises(verify.VerificationError):
                verify.build_path_roles(Path("/source"), Path("/ndk"), ANDROID_PAGE_SIZE)

    def test_candidate_workflow_is_fixed_private_manual_readonly_and_pinned(self):
        workflow = (Path(__file__).resolve().parents[3] / ".github/workflows/android-page-size-reproduction.yml").read_text()
        self.assertIn("  workflow_dispatch:\n", workflow)
        self.assertNotIn("inputs:", workflow)
        self.assertNotIn("secrets.", workflow)
        self.assertNotIn("cache@", workflow)
        self.assertIn("github.event.repository.private", workflow)
        self.assertIn("github.ref == 'refs/heads/master'", workflow)
        self.assertIn("runs-on: xcode-27", workflow)
        self.assertIn("contents: read", workflow)
        self.assertIn("ref: " + ANDROID_PAGE_SIZE.source_commit, workflow)
        self.assertEqual(workflow.count("--profile " + ANDROID_PAGE_SIZE.name), 2)
        pins = re.findall(r"uses: (\S+)", workflow)
        self.assertEqual(len(pins), 3)
        self.assertTrue(all(re.fullmatch(r"actions/(?:checkout|upload-artifact)@[0-9a-f]{40}", value) for value in pins))

    def test_unknown_cli_profile_fails_before_evidence_creation(self):
        script = Path(__file__).resolve().parents[1] / "build_current_pin.py"
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "evidence"
            result = subprocess.run([sys.executable, str(script), "--profile", "../../arbitrary.json",
                                     "--source-dir", directory, "--evidence-dir", str(evidence)], capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertFalse(evidence.exists())


if __name__ == "__main__":
    unittest.main()
