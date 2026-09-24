# SPDX-License-Identifier: MIT OR Apache-2.0
"""Execute the real build script for cross-compilation target metadata."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class AndroidLinkPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="sdr-fox-link-policy-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.executable = Path(cls.temporary.name) / "link-policy"
        subprocess.run(["rustc", "--edition=2021", "--deny", "warnings",
                        str(ROOT / "crates/sdr-fox-jni/build.rs"),
                        "-o", str(cls.executable)], check=True, capture_output=True)

    def run_policy(self, target):
        env = os.environ.copy()
        env.pop("CARGO_CFG_TARGET_OS", None)
        if target is not None:
            env["CARGO_CFG_TARGET_OS"] = target
        return subprocess.run([str(self.executable)], env=env, text=True,
                              capture_output=True, check=False)

    def test_android_cross_target_emits_both_cdylib_flags(self):
        result = self.run_policy("android")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines(), [
            "cargo:rerun-if-changed=build.rs",
            "cargo:rustc-link-arg-cdylib=-Wl,-z,max-page-size=16384",
            "cargo:rustc-link-arg-cdylib=-Wl,-z,common-page-size=16384",
        ])

    def test_non_android_targets_never_receive_elf_flags(self):
        for target in ("macos", "ios", "linux", "windows"):
            with self.subTest(target=target):
                result = self.run_policy(target)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout.splitlines(),
                                 ["cargo:rerun-if-changed=build.rs"])

    def test_missing_target_metadata_fails_instead_of_guessing_host(self):
        result = self.run_policy(None)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("rustc-link-arg", result.stdout)


if __name__ == "__main__":
    unittest.main()
