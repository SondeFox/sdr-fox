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
from unittest.mock import Mock, patch

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
                           ("CARGO_HOME", "/cargo"), ("RUSTFLAGS", "-D warnings"), ("RUSTC", "/unreviewed/rustc"),
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


class MetadataPrefetchTests(unittest.TestCase):
    def worker(self, directory):
        root = Path(directory)
        header = root / "bindings/sdr_fox.h"
        header.parent.mkdir()
        header.write_bytes(b"original reviewed header\n")
        return build.Builder(root, root / "evidence", {}), header

    def test_metadata_only_locked_target_dependency_is_fetched_before_offline_query(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, header = self.worker(directory)
            cache = set()
            commands = []
            # Model the observed cold-host failure: this Windows package is
            # locked but is absent from Mac/Android-only fetches. No real Cargo
            # process, registry download, or generated dependency is used here.
            metadata_only_package = ("anstyle-wincon", "3.0.11")
            def cargo(label, argv, **kwargs):
                commands.append((label, argv))
                self.assertIn("--locked", argv)
                if argv[1] == "fetch":
                    self.assertNotIn("CARGO_NET_OFFLINE", worker.env)
                    if "--target" not in argv:
                        cache.add(metadata_only_package)
                elif argv[1] == "metadata":
                    self.assertEqual(worker.env["CARGO_NET_OFFLINE"], "true")
                    self.assertIn("--offline", argv)
                    self.assertIn("--all-features", argv)
                    self.assertNotIn("--filter-platform", argv)
                    self.assertNotIn("--no-deps", argv)
                    self.assertEqual(argv[-1], str(worker.source / "crates/sdr-fox-cabi/Cargo.toml"))
                    if metadata_only_package not in cache:
                        raise build.BuildError("missing locked metadata-only package")
                else:
                    self.fail("Unexpected compilation during metadata preparation")
                return ""
            worker.command = cargo
            worker.prefetch_locked_metadata()
            self.assertEqual([label for label, _ in commands], ["fetch-locked-crates", "preflight-cabi-metadata"])
            self.assertEqual(header.read_bytes(), b"original reviewed header\n")
            self.assertEqual(worker.env["CARGO_NET_OFFLINE"], "true")

    def test_fetch_failure_does_not_query_metadata_remove_header_or_compile(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, header = self.worker(directory)
            compiler = Mock()
            worker.command = Mock(side_effect=build.BuildError("controlled locked fetch failure"))
            with self.assertRaises(build.BuildError):
                worker.prefetch_locked_metadata()
                build.build_regenerating_header(header, compiler)
            self.assertEqual(worker.command.call_count, 1)
            compiler.assert_not_called()
            self.assertEqual(header.read_bytes(), b"original reviewed header\n")
            self.assertEqual(worker.receipt["artifacts"], [])

    def test_offline_metadata_failure_preserves_header_and_prevents_build(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, header = self.worker(directory)
            compiler = Mock()
            worker.command = Mock(side_effect=["", build.BuildError("controlled offline metadata failure")])
            with self.assertRaises(build.BuildError):
                worker.prefetch_locked_metadata()
                build.build_regenerating_header(header, compiler)
            self.assertEqual(worker.command.call_count, 2)
            compiler.assert_not_called()
            self.assertEqual(header.read_bytes(), b"original reviewed header\n")
            self.assertEqual(worker.env["CARGO_NET_OFFLINE"], "true")
            self.assertEqual(worker.receipt["artifacts"], [])


class AndroidCompilerBindingTests(unittest.TestCase):
    def worker(self, directory, version=build.RUSTC_VERSION):
        root = Path(directory)
        worker = build.Builder(root, root / "evidence", {"HOME": str(root), "PATH": "/unselected/bin"})
        rustup = root / "rustup"
        compiler = rustup / "toolchains" / build.RUST_TOOLCHAIN / "bin/rustc"
        compiler.parent.mkdir(parents=True)
        compiler.write_bytes(b"synthetic compiler bytes; never executed")
        worker.env.update(RUSTUP_HOME=str(rustup), CARGO_NET_OFFLINE="true", RUSTFLAGS="reviewed remaps unchanged")
        worker.roles["$RUSTUP_HOME"] = str(rustup)
        inventory = [{"path": str(compiler.relative_to(rustup)), "sha256": build.sha256(compiler)}]
        (worker.evidence / "rust-toolchain-inventory.json").write_text(json.dumps(inventory))
        calls = []
        def command(label, argv, **kwargs):
            calls.append((label, argv, kwargs))
            if label == "android-rustc-version":
                raw = (version + "\n").encode()
                log = worker.evidence / "logs/android-rustc-version.log"
                log.write_bytes(raw)
                worker.receipt["commands"].append({"label": label, "status": "pass", "exit_code": 0,
                    "log": str(log.relative_to(worker.evidence)), "log_sha256": hashlib.sha256(raw).hexdigest()})
                return raw.decode()
            self.assertEqual(label, "build-android")
            return "synthetic Android diagnostics"
        worker.command = command
        return worker, compiler, calls

    def test_only_android_child_gets_exact_directly_verified_compiler(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, compiler, calls = self.worker(directory)
            parent_before, original_before = dict(worker.env), dict(worker.original_env)
            self.assertEqual(worker.build_android(compiler.parent), "synthetic Android diagnostics")
            self.assertEqual([call[0] for call in calls], ["android-rustc-version", "build-android"])
            self.assertEqual(calls[0][1], [str(compiler), "--version"])
            self.assertEqual(calls[1][2]["env"], parent_before | {"RUSTC": str(compiler)})
            self.assertEqual(worker.env, parent_before)
            self.assertEqual(worker.original_env, original_before)
            self.assertNotIn("RUSTC", worker.env)
            self.assertEqual(worker.receipt["android_compiler"]["path"], "$RUSTUP_HOME/toolchains/" + build.RUST_TOOLCHAIN + "/bin/rustc")
            self.assertEqual(worker.receipt["android_compiler"]["sha256"], build.sha256(compiler))
            self.assertEqual(worker.receipt["android_compiler"]["version"], build.RUSTC_VERSION)

    def test_missing_or_wrong_selected_compiler_stops_before_build(self):
        for mode in ("missing", "wrong-directory", "relative", "inventory-mismatch", "missing-inventory-entry", "parent-override"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                worker, compiler, calls = self.worker(directory)
                selected = compiler.parent
                if mode == "missing":
                    compiler.unlink()
                elif mode == "wrong-directory":
                    selected = selected.parent / "other-bin"
                elif mode == "relative":
                    selected = Path("relative/bin")
                elif mode == "inventory-mismatch":
                    compiler.write_bytes(b"changed after toolchain inventory")
                elif mode == "missing-inventory-entry":
                    (worker.evidence / "rust-toolchain-inventory.json").write_text("[]")
                else:
                    worker.env["RUSTC"] = "/unreviewed/rustc"
                with self.assertRaises(build.BuildError):
                    worker.build_android(selected)
                self.assertEqual(calls, [])
                self.assertNotIn("android_compiler", worker.receipt)

    def test_wrong_direct_version_stops_before_android_build(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, compiler, calls = self.worker(directory, version="rustc 1.96.0 (synthetic wrong version)")
            with self.assertRaises(build.BuildError):
                worker.build_android(compiler.parent)
            self.assertEqual([call[0] for call in calls], ["android-rustc-version"])
            self.assertNotIn("RUSTC", worker.env)
            self.assertNotIn("android_compiler", worker.receipt)


if __name__ == '__main__':
    unittest.main()
