# SPDX-License-Identifier: MIT OR Apache-2.0
"""Separate host/runtime stripping and keep previous reconstruction subjects intact."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build_current_pin as build
import verify_current_pin as verify
from host_build_policy import inspect_macos_build, verify_target_strip
from candidate_authority import projection
from profiles import (ANDROID_PAGE_SIZE, ANDROID_PAGE_SIZE_HOSTSAFE, HISTORICAL,
                      HOST_BUILD_STRIP_CONFIG, Manifest, build_override_args, load_manifest)


COMPILER = "/reviewed/toolchain/bin/rustc"
LOG = (f"Running `{COMPILER} --crate-name thiserror_impl --crate-type proc-macro -C opt-level=0`\n"
       f"Running `{COMPILER} --crate-name build_script_build --crate-type bin`\n"
       f"Running `{COMPILER} --crate-name sdr_fox --crate-type staticlib --crate-type rlib --target aarch64-apple-darwin -C strip=symbols`\n")


class HostBuildPolicyTests(unittest.TestCase):
    def test_old_manifests_and_profiles_keep_their_exact_policy(self):
        self.assertEqual(load_manifest().sha256, "8a21907438c9aaa3c25e5e9186c36bf45519b87abe2f428623129c488c33b937")
        self.assertEqual(load_manifest(ANDROID_PAGE_SIZE).sha256, "bd4e4373c9dbcee65a26163d6d87f28af2b93e8819539312845bdb11f3c51cb2")
        for profile in (HISTORICAL, ANDROID_PAGE_SIZE):
            self.assertEqual(build_override_args(profile), [])
        self.assertEqual(build_override_args(ANDROID_PAGE_SIZE_HOSTSAFE),
                         ["--config", 'profile.release.build-override.strip="none"'])

    def test_new_manifest_cannot_be_confused_with_original_candidate(self):
        original, new = load_manifest(ANDROID_PAGE_SIZE), load_manifest(ANDROID_PAGE_SIZE_HOSTSAFE)
        self.assertNotEqual(original.sha256, new.sha256)
        self.assertEqual(new.document["cargo_build_override"], HOST_BUILD_STRIP_CONFIG)
        with self.assertRaises(ValueError):
            Manifest(ANDROID_PAGE_SIZE_HOSTSAFE, original.raw)
        with self.assertRaises(ValueError):
            Manifest(ANDROID_PAGE_SIZE, new.raw)
        before = {x["role"]: x["sha256"] for x in original.document["artifacts"]}
        after = {x["role"]: x["sha256"] for x in new.document["artifacts"]}
        self.assertEqual({key for key in before if before[key] != after[key]},
                         {"macos_archive", "android_arm64_jni", "android_x86_64_jni"})

    def test_builder_scopes_new_config_and_direct_compiler_to_hostsafe_children(self):
        for profile in (HISTORICAL, ANDROID_PAGE_SIZE, ANDROID_PAGE_SIZE_HOSTSAFE):
            with self.subTest(profile=profile.name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                worker = build.Builder(root, root / "evidence", {"PATH": "/base"}, profile)
                parent = dict(worker.env)
                worker.command = Mock(return_value="synthetic build log")
                worker.verified_compiler = Mock(return_value=Path(COMPILER))
                worker.build_macos(Path("/reviewed/toolchain/bin"))
                _, args = worker.command.call_args.args
                if profile == ANDROID_PAGE_SIZE_HOSTSAFE:
                    self.assertEqual(args[:5], ["cargo", "build", "--config", HOST_BUILD_STRIP_CONFIG, "-vv"])
                    self.assertEqual(worker.command.call_args.kwargs["env"]["RUSTC"], COMPILER)
                    worker.verified_compiler.assert_called_once_with(Path("/reviewed/toolchain/bin"), "macos")
                    self.assertEqual(worker.receipt["cargo_build_override"], HOST_BUILD_STRIP_CONFIG)
                else:
                    self.assertEqual(args, ["cargo", "build", "--locked", "--offline", "--release", "--target", "aarch64-apple-darwin", "-p", "sdr-fox-cabi"])
                    self.assertNotIn("RUSTC", worker.command.call_args.kwargs["env"])
                    worker.verified_compiler.assert_not_called()
                    self.assertNotIn("cargo_build_override", worker.receipt)
                worker.build_android(Path("/reviewed/toolchain/bin"))
                android = worker.command.call_args.args[1]
                self.assertEqual(android.count("--config"), int(profile == ANDROID_PAGE_SIZE_HOSTSAFE))
                if profile == ANDROID_PAGE_SIZE_HOSTSAFE:
                    self.assertEqual(android[android.index("--config") + 1], HOST_BUILD_STRIP_CONFIG)
                self.assertEqual(worker.env, parent)

    def test_real_compiler_log_requires_host_none_and_runtime_symbols(self):
        observed = inspect_macos_build(LOG, COMPILER)
        self.assertEqual(observed["proc_macro_crates"], ["thiserror_impl"])
        self.assertEqual(observed["host_strip"], "none")
        self.assertEqual(observed["runtime_strip"], "symbols")
        self.assertEqual(inspect_macos_build(LOG.replace("-C opt-level=0", "-C opt-level=0 -C strip=none"), COMPILER), observed)

    def test_stripped_host_or_unstripped_runtime_is_rejected(self):
        for changed in (LOG.replace("-C opt-level=0", "-C strip=symbols"),
                        LOG.replace("-C opt-level=0", "-C strip=debuginfo"),
                        LOG.replace("-C strip=symbols", "-C strip=none"),
                        LOG.replace("--crate-type bin", "--crate-type bin -C strip=symbols")):
            with self.subTest(log=changed), self.assertRaises(ValueError):
                inspect_macos_build(changed, COMPILER)

    def test_missing_duplicate_wrong_target_or_wrong_compiler_cannot_pass(self):
        for changed in ("", LOG.replace("--crate-name thiserror_impl", "--crate-name other"),
                        LOG + LOG.splitlines()[0] + "\n",
                        LOG + "Running `rustc --crate-name other --crate-type proc-macro -C strip=symbols`\n",
                        LOG.replace("-C opt-level=0", "--target aarch64-apple-darwin"),
                        LOG.replace(COMPILER, "/unreviewed/bin/rustc")):
            with self.subTest(log=changed), self.assertRaises(ValueError):
                inspect_macos_build(changed, COMPILER)

    def test_target_strip_must_be_explicit_and_unambiguous(self):
        verify_target_strip(["rustc", "-Cstrip=symbols"])
        verify_target_strip(["rustc", "--codegen=strip=symbols"])
        for args in ([], ["-C", "strip=none"], ["-C", "strip=symbols", "-C", "strip=none"], ["-C"]):
            with self.subTest(args=args), self.assertRaises(ValueError):
                verify_target_strip(args)

    def test_android_receipt_rejects_absent_global_or_duplicate_override_before_parse(self):
        profile = ANDROID_PAGE_SIZE_HOSTSAFE
        command = ["cargo", "ndk", "--platform", "21", "-t", "arm64-v8a", "-t", "x86_64", "build",
                   "--config", HOST_BUILD_STRIP_CONFIG, "-vv", "--color", "never", "--locked", "--offline", "--release", "-p", "sdr-fox-jni", "--features", "android"]
        receipt = {"android_effective_commands": [{}, {}]}
        wrong = [command[:9] + command[11:],
                 [value.replace("release.build-override.strip", "release.strip") for value in command],
                 command + ["--config", HOST_BUILD_STRIP_CONFIG]]
        for argv in wrong:
            with self.subTest(argv=argv), patch.object(verify, "checked_command", return_value=(argv, "")), patch.object(verify, "parse_android_build") as parse, self.assertRaisesRegex(verify.VerificationError, "android-build-command"):
                verify.verify_android_observations(receipt, Path("/evidence"), {}, {}, profile)
            parse.assert_not_called()

    def test_mac_receipt_rejects_global_strip_override_before_inspection(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE_HOSTSAFE)
        receipt = {"reconstruction_profile": manifest.profile.name, "expected_manifest_sha256": manifest.sha256,
                   "cargo_build_override": HOST_BUILD_STRIP_CONFIG,
                   "runner": {"GITHUB_RUN_ID": "1234", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "a" * 40},
                   "authority": projection(1234, 1, "a" * 40)}
        command = ["cargo", "build", "--config", 'profile.release.strip="none"', "-vv", "--locked", "--offline",
                   "--release", "--target", "aarch64-apple-darwin", "-p", "sdr-fox-cabi"]
        with patch.object(verify, "read_build_receipt", return_value=(receipt, "a" * 64)), patch.object(verify, "build_path_roles", return_value={}), patch.object(verify, "checked_command", side_effect=[(["xcodebuild", "-version"], manifest.profile.xcode_identity), (command, LOG)]), patch.object(verify, "inspect_macos_build") as inspect, self.assertRaisesRegex(verify.VerificationError, "macos-build-command"):
            verify.verify_build_evidence(Path("/evidence/build-receipt.json"), Path("/source"), Path("/artifacts"), Path("/ndk"), manifest.document, manifest=manifest)
        inspect.assert_not_called()


if __name__ == "__main__":
    unittest.main()
