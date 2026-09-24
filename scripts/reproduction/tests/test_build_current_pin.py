# SPDX-License-Identifier: MIT OR Apache-2.0
"""Guard/failure tests; never bootstrap a toolchain or compile native code."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("build_current_pin", Path(__file__).resolve().parents[1] / "build_current_pin.py")
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)


class HostTests(unittest.TestCase):
    def valid(self):
        return {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "macOS", "RUNNER_ARCH": "ARM64", "GITHUB_REPOSITORY": "SondeFox/sdr-fox",
                "GITHUB_EVENT_NAME": "workflow_dispatch",
                "GITHUB_REF": "refs/heads/master", "REPRO_PRIVATE_REPOSITORY": "true"}

    @patch.object(build.platform, "machine", return_value="arm64")
    @patch.object(build.platform, "system", return_value="Darwin")
    def test_wrong_host_and_override_rejected(self, *_):
        build.guard_host(self.valid())
        for key, value in (("RUNNER_ENVIRONMENT", "self-hosted"), ("GITHUB_REPOSITORY", "fork/sdr-fox"),
                           ("GITHUB_REF", "refs/heads/candidate"), ("GITHUB_EVENT_NAME", "pull_request"), ("REPRO_PRIVATE_REPOSITORY", "false"),
                           ("CARGO_HOME", "/cargo"), ("RUSTFLAGS", "-D warnings"),
                           ("GIT_CONFIG_COUNT", "1"), ("CARGO_BUILD_RUSTC_WRAPPER", "/wrapper")):
            with self.subTest(key=key), self.assertRaises(build.BuildError):
                build.guard_host(self.valid() | {key: value})

    def test_local_invocation_cannot_create_evidence_or_move_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.dict(os.environ, {}, clear=True), patch.object(sys, "argv", ["build", "--source-dir", str(root), "--evidence-dir", str(root / "evidence")]):
                self.assertEqual(build.main(), 1)
            self.assertFalse((root / "evidence").exists())


class SourceTests(unittest.TestCase):
    def test_source_identity_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lock = b"controlled test lock\n"
            (root / "Cargo.lock").write_bytes(lock)
            responses = {
                ("git", "remote"): "origin\n",
                ("git", "remote", "get-url", "--all", "origin"): "https://github.com/SondeFox/sdr-fox.git\n",
                ("git", "remote", "get-url", "--push", "--all", "origin"): "https://github.com/SondeFox/sdr-fox.git\n",
                ("git", "rev-parse", "HEAD"): build.SOURCE_COMMIT + "\n",
                ("git", "rev-parse", "HEAD^{tree}"): build.SOURCE_TREE + "\n",
                ("git", "rev-list", "--max-parents=0", "HEAD"): build.SOURCE_ROOT + "\n",
                ("git", "status", "--porcelain", "--untracked-files=all"): "",
            }
            with patch.object(build, "LOCK_SHA256", hashlib.sha256(lock).hexdigest()):
                build.guard_source(root, lambda args: responses[tuple(args)])
                for key, bad in ((('git', 'remote'), 'origin\nother\n'),
                                 (('git', 'remote', 'get-url', '--push', '--all', 'origin'), 'https://example.org/wrong.git\n'),
                                 (('git', 'rev-parse', 'HEAD'), '0' * 40),
                                 (('git', 'rev-parse', 'HEAD^{tree}'), '0' * 40),
                                 (('git', 'rev-list', '--max-parents=0', 'HEAD'), build.SOURCE_COMMIT),
                                 (('git', 'status', '--porcelain', '--untracked-files=all'), '?? injected.rs\n')):
                    with self.subTest(command=key), self.assertRaises(build.BuildError):
                        build.guard_source(root, lambda args: bad if tuple(args) == key else responses[tuple(args)])
                (root / "target").mkdir()
                with self.assertRaises(build.BuildError):
                    build.guard_source(root, lambda args: responses[tuple(args)])


class CacheTests(unittest.TestCase):
    def test_cache_is_quarantined_not_reused_and_tools_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); home = root / "home"
            (home / ".cargo" / "registry").mkdir(parents=True)
            (home / ".cargo" / "registry" / "old").write_text("old cached input")
            (home / ".cargo" / "bin").mkdir()
            moved = build.quarantine_cargo_cache(home, root / "quarantine")
            self.assertEqual(moved, ["registry"])
            self.assertFalse((home / ".cargo" / "registry").exists())
            self.assertTrue((home / ".cargo" / "bin").is_dir())
            self.assertEqual((root / "quarantine" / "registry" / "old").read_text(), "old cached input")

    def test_cache_config_and_symlinks_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); home = root / "home"
            (home / ".cargo").mkdir(parents=True)
            config = home / ".cargo" / "config.toml"; config.write_text("[build]\n")
            with self.assertRaises(build.BuildError):
                build.quarantine_cargo_cache(home, root / "quarantine")
            config.unlink()
            (home / ".cargo" / "registry").symlink_to(root)
            with self.assertRaises(build.BuildError):
                build.quarantine_cargo_cache(home, root / "quarantine")

    def test_exact_remap_order_keeps_default_cache_below_home(self):
        self.assertEqual(build.remap_flags(Path('/source'), Path('/builder')),
                         '--remap-path-prefix=/source=/workspace/sdr-fox --remap-path-prefix=/builder/.cargo=/cargo --remap-path-prefix=/builder=/home/builder')


class FailureEvidenceTests(unittest.TestCase):
    def test_silent_header_generation_failure_cannot_reuse_checked_in_header(self):
        with tempfile.TemporaryDirectory() as directory:
            header = Path(directory) / 'sdr_fox.h'; header.write_text('reviewed header')
            with self.assertRaises(build.BuildError):
                build.build_regenerating_header(header, lambda: None)
            self.assertFalse(header.exists())

    def test_regeneration_requires_identical_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            header = Path(directory) / 'sdr_fox.h'; header.write_text('reviewed header')
            def successful_compile():
                self.assertFalse(header.exists())
                header.write_text('reviewed header')
            build.build_regenerating_header(header, successful_compile)
            with self.assertRaises(build.BuildError):
                build.build_regenerating_header(header, lambda: header.write_text('unexpected header'))

    def test_failed_command_keeps_nonzero_log_and_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            worker = build.Builder(root, root / "evidence", {"PATH": os.environ["PATH"], "HOME": str(root)})
            with self.assertRaises(build.BuildError):
                worker.command("controlled-failure", [sys.executable, '-c', 'print("failure detail"); raise SystemExit(7)'])
            receipt = json.loads((root / "evidence" / "build-receipt.json").read_text())
            self.assertEqual(receipt['commands'][0]['exit_code'], 7)
            self.assertEqual(receipt['commands'][0]['status'], 'failed')
            self.assertIn('failure detail', (root / "evidence" / receipt['commands'][0]['log']).read_text())

    def test_preexisting_evidence_cannot_be_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); evidence = root / "evidence"; evidence.mkdir()
            (evidence / 'keep').write_text('existing proof')
            with self.assertRaises(FileExistsError):
                build.Builder(root, evidence, {})
            self.assertEqual((evidence / 'keep').read_text(), 'existing proof')

    def test_log_overflow_is_bounded_and_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); worker = build.Builder(root, root / 'evidence', {"PATH": os.environ["PATH"]})
            with patch.object(build, "LOG_LIMIT", 32), self.assertRaises(build.BuildError):
                worker.command('oversized-log', [sys.executable, '-c', 'print("x" * 500)'])
            entry = worker.receipt['commands'][0]
            self.assertEqual(entry['status'], 'log_limit_exceeded')
            self.assertLess((worker.evidence / entry['log']).stat().st_size, 128)

    def test_timeout_keeps_failure_log_and_hash(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); worker = build.Builder(root, root / 'evidence', {"PATH": os.environ["PATH"]})
            worker.deadline = build.time.monotonic() + 0.2
            with self.assertRaises(build.BuildError):
                worker.command('timeout', [sys.executable, '-c', 'import time; print("started", flush=True); time.sleep(10)'])
            entry = worker.receipt['commands'][0]
            self.assertEqual(entry['status'], 'timeout')
            self.assertEqual(entry['log_sha256'], build.sha256(worker.evidence / entry['log']))

    def test_missing_or_symlink_artifact_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); worker = build.Builder(root, root / 'evidence', {})
            with self.assertRaises(build.BuildError):
                worker.copy_artifact(root / 'absent', 'libsdr_fox_ffi.a')
            (root / 'file').write_text('test'); (root / 'link').symlink_to(root / 'file')
            with self.assertRaises(build.BuildError):
                worker.copy_artifact(root / 'link', 'libsdr_fox_ffi.a')


if __name__ == '__main__':
    unittest.main()
