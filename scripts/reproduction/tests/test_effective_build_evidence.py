# SPDX-License-Identifier: MIT OR Apache-2.0
"""Reject absent/contradictory measurements without a cold native build."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("build_effective", Path(__file__).resolve().parents[1] / "build_current_pin.py")
build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build)

CLANG = Path("/ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin/clang")
LINKER = Path("/tools/bin/cargo-ndk")
RUSTC = Path("/rust/bin/rustc")


def android_build_log():
    lines = []
    for target, abi in ((build.TARGETS[1], "arm64-v8a"), (build.TARGETS[2], "x86_64")):
        lines += [f"    Building {abi} ({target})", "    Exporting ANDROID_PLATFORM=21",
                  f'    Exporting ANDROID_ABI="{abi}"',
                  f'    Exporting _CARGO_NDK_LINK_CLANG="{CLANG}"',
                  f'    Exporting _CARGO_NDK_LINK_TARGET="--target={target}21"',
                  f'    Exporting CARGO_TARGET_{target.upper().replace("-", "_")}_LINKER="{LINKER}"',
                  f"    Running `CARGO_PKG_NAME=sdr-fox-jni RUSTC={RUSTC} {RUSTC} --crate-name sdr_fox_jni --crate-type cdylib --crate-type rlib --target {target} -C linker={LINKER}`"]
    return "\n".join(lines) + "\n"


def clang_trace(target):
    tools = "/ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin"
    library = tools + "/../sysroot/usr/lib/" + target + "/21"
    triple = target.replace("-linux-", "-unknown-linux-") + "21"
    commands = [[tools + "/clang", "-cc1", "-triple", triple, "-x", "c", "/scratch/android-driver.c"],
                [tools + "/ld.lld", "-shared", library + "/crtbegin_so.o", "-L" + library,
                 "/scratch/probe.o", "-lc", library + "/crtend_so.o"]]
    return "Target: " + triple + "\n" + "\n".join(" ".join(json.dumps(x) for x in argv) for argv in commands) + "\n"


class EffectiveAndroidTests(unittest.TestCase):
    def test_two_measured_compiler_wrapper_and_api_sets_agree(self):
        rows = build.parse_android_build(android_build_log(), CLANG, LINKER, RUSTC)
        self.assertEqual([row["rust_target"] for row in rows], list(build.TARGETS[1:]))
        self.assertEqual([row["api_level"] for row in rows], [21, 21])

    def test_missing_or_contradictory_observed_values_fail(self):
        original = android_build_log()
        variants = [original.split("    Building x86_64")[0],
                    original.replace("    Exporting ANDROID_PLATFORM=21\n", "", 1),
                    original.replace("ANDROID_PLATFORM=21", "ANDROID_PLATFORM=22", 1),
                    original.replace('--target=aarch64-linux-android21', '--target=aarch64-linux-android23', 1),
                    original.replace('ANDROID_ABI="arm64-v8a"', 'ANDROID_ABI="x86_64"', 1),
                    original.replace('ANDROID_PLATFORM=21\n', 'ANDROID_PLATFORM=21\n    Exporting ANDROID_PLATFORM=22\n', 1),
                    original.replace(f"-C linker={LINKER}", "-C linker=/wrong/linker", 1),
                    original.replace(f"{RUSTC} --crate-name", "/wrong/rustc --crate-name", 1),
                    original.replace(f"{RUSTC} --crate-name", "rustc --crate-name", 1),
                    original.replace("--target aarch64-linux-android", "--target x86_64-linux-android", 1),
                    original.replace("--crate-type cdylib", "--crate-type staticlib", 1)]
        for index, log in enumerate(variants):
            with self.subTest(index=index), self.assertRaises(build.BuildError):
                build.parse_android_build(log, CLANG, LINKER, RUSTC)

    def test_clang_shared_link_normalizes_bin_parent_paths(self):
        for target in build.TARGETS[1:]:
            observed = build.parse_android_clang(clang_trace(target), target, Path("/ndk"))
            self.assertEqual(observed["api_level"], 21)
            self.assertNotIn("/../", observed["platform_library_path"])
            self.assertEqual(len(observed["crt_objects"]), 2)

    def test_clang_missing_or_conflicting_api_and_tools_fail(self):
        original = clang_trace(build.TARGETS[1])
        variants = [original.replace('"-cc1"', '"-wrong"'), original.replace('"-shared"', '"-pie"'),
                    original.replace('"aarch64-unknown-linux-android21"', '"aarch64-unknown-linux-android22"', 1),
                    original.replace('Target: aarch64-unknown-linux-android21', 'Target: aarch64-unknown-linux-android22'),
                    original.replace('/21/crtbegin_so.o', '/22/crtbegin_so.o'),
                    original.replace('/21/crtend_so.o', '/21/crtend_android.o'),
                    original.replace('"-L/ndk', '"-X/ndk'),
                    original.replace('/bin/clang"', '/bin/other-clang"'),
                    original.replace('/bin/ld.lld"', '/other/ld.lld"')]
        for index, log in enumerate(variants):
            with self.subTest(index=index), self.assertRaises(build.BuildError):
                build.parse_android_clang(log, build.TARGETS[1], Path("/ndk"))


class AppleIdentityTests(unittest.TestCase):
    def test_actual_apple_identity_required(self):
        build.validate_apple_version("clang", "Apple clang version 21.0.0")
        build.validate_apple_version("nm", "llvm-nm, compatible with GNU nm\nApple LLVM version 21.0.0")
        for tool, version in (("clang", ""), ("clang", "clang version 21"), ("nm", "GNU nm"), ("nm", "llvm-nm")):
            with self.subTest(tool=tool, version=version), self.assertRaises(build.BuildError):
                build.validate_apple_version(tool, version)

    def test_missing_or_contradictory_kernel_fields_fail(self):
        valid = {"system": "Darwin", "release": "25.6.0", "version": "Darwin Kernel Version 25.6.0: build", "machine": "arm64"}
        build.validate_darwin(valid)
        for change in ({"release": ""}, {"version": "Darwin Kernel Version 24.0.0: other"}, {"machine": "x86_64"}, {"system": "Linux"}):
            with self.assertRaises(build.BuildError):
                build.validate_darwin(valid | change)


class SmokeFailureTests(unittest.TestCase):
    def run_smoke(self, directory, *, fail=None, output=b"0.1.0\n"):
        root = Path(directory)
        scratch = root / "scratch"; scratch.mkdir()
        archive = root / "libsdr_fox_ffi.a"; archive.write_bytes(b"synthetic archive, never linked")
        header = root / "sdr_fox.h"; header.write_bytes(b"synthetic header, never compiled")
        worker = build.Builder(root, root / "evidence", {})
        calls = []
        def command(label, argv, **kwargs):
            calls.append((label, argv))
            if label == fail:
                raise build.BuildError("controlled tool failure")
            raw = str(root).encode() + b"\n" if label == "apple-sdk-path" else output if label == "macos-c-run-smoke" else b""
            if label == "macos-c-link-smoke":
                (scratch / "sdr-version-smoke").write_bytes(b"synthetic executable, never run")
            log = worker.evidence / "logs" / (label + ".log"); log.write_bytes(raw)
            worker.receipt["commands"].append({"label": label, "status": "pass", "exit_code": 0,
                "log": str(log.relative_to(worker.evidence)), "log_sha256": hashlib.sha256(raw).hexdigest()})
            return raw.decode()
        worker.command = command
        try:
            worker.c_link_smoke(archive, header, scratch, Path("/clang"))
        except build.BuildError:
            pass
        return worker, calls

    def test_link_failure_prevents_run_and_fails_proof(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, calls = self.run_smoke(directory, fail="macos-c-link-smoke")
            self.assertEqual(worker.receipt["c_link_smoke"]["status"], "failed")
            self.assertNotIn("macos-c-run-smoke", [name for name, _ in calls])

    def test_run_failure_and_wrong_output_fail_proof(self):
        for options in ({"fail": "macos-c-run-smoke"}, {"output": b"0.2.0\n"}, {"output": b"0.1.0\r\n"}):
            with tempfile.TemporaryDirectory() as directory:
                worker, _ = self.run_smoke(directory, **options)
                self.assertEqual(worker.receipt["c_link_smoke"]["status"], "failed")

    def test_success_binds_actual_inputs_executable_and_output(self):
        with tempfile.TemporaryDirectory() as directory:
            worker, calls = self.run_smoke(directory)
            receipt = worker.receipt["c_link_smoke"]
            self.assertEqual(receipt["status"], "pass")
            self.assertEqual(receipt["version"], "0.1.0")
            self.assertEqual(receipt["source_sha256"], "4c6300c5b9cc8d8f779e2f024349c7f67b22973f09f9411491f2bfbfc925539a")
            self.assertEqual(receipt["stdout_sha256"], hashlib.sha256(b"0.1.0\n").hexdigest())
            self.assertEqual(receipt["archive_sha256"], build.sha256(Path(directory) / "libsdr_fox_ffi.a"))
            link = next(argv for name, argv in calls if name == "macos-c-link-smoke")
            self.assertIn("-isysroot", link)
            self.assertIn(str(Path(directory) / "libsdr_fox_ffi.a"), link)
            self.assertFalse((worker.evidence / "artifacts/sdr-version-smoke").exists())


if __name__ == "__main__":
    unittest.main()
