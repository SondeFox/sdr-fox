"""Synthetic verifier failures; no captured/private bytes or native compilation."""
import copy
import importlib.util
import itertools
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import patch

MODULE_PATH = Path(__file__).resolve().parents[1] / "verify_current_pin.py"
spec = importlib.util.spec_from_file_location("verify_current_pin", MODULE_PATH)
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)
EXPECTED = json.loads(MODULE_PATH.with_name("expected.json").read_text())


def synthetic_elf(machine=183):
    data = bytearray(64)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HH", data, 16, 3, machine)
    return bytes(data)


def synthetic_archive(cpu=16777228, minimum=14 << 16):
    member = struct.pack("<8I", 0xFEEDFACF, cpu, 0, 1, 1, 24, 0, 0)
    member += struct.pack("<6I", 0x32, 24, 1, minimum, 0, 0)
    header = b"object.o/".ljust(16) + b"0".ljust(12) + b"0".ljust(6) + b"0".ljust(6) + b"100644".ljust(8) + str(len(member)).encode().ljust(10) + b"`\n"
    return b"!<arch>\n" + header + member


class VerifierTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.addCleanup(self.temp.cleanup)

    def fails(self, code, fn):
        with self.assertRaisesRegex(v.VerificationError, "^" + code + "$"):
            fn()

    def test_nonzero_inspector_with_convincing_stdout_fails(self):
        fake = subprocess.CompletedProcess([], 1, "arm64\n", "private/path/token")
        with patch.object(v.subprocess, "run", return_value=fake):
            self.fails("inspection-command-failed", lambda: v.run(["lipo"]))

    def test_empty_successful_inspector_fails(self):
        with patch.object(v.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")):
            self.fails("inspection-command-empty", lambda: v.run(["nm"]))

    def test_inspector_timeout_is_sanitized(self):
        with patch.object(v.subprocess, "run", side_effect=subprocess.TimeoutExpired("private-secret", 180)):
            self.fails("inspection-command-unavailable", lambda: v.run(["nm"]))

    def test_unsafe_filename_and_symlink_rejected(self):
        p = self.root / "actual"
        p.write_text("test")
        (self.root / "alias").symlink_to(p)
        self.fails("unsafe-input-name", lambda: v.safe_file(self.root, "../actual"))
        self.fails("unsafe-input-name", lambda: v.safe_file(self.root, "/actual"))
        self.fails("symlink-input", lambda: v.safe_file(self.root, "alias"))

    def test_all_five_filenames_required_and_no_extras(self):
        for artifact in EXPECTED["artifacts"]:
            p = self.root / artifact["path"]
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b"synthetic")
        v.validate_artifact_layout(self.root, EXPECTED["artifacts"])
        (self.root / "private-receipt.json").write_text("{}")
        self.fails("artifact-file-set-mismatch", lambda: v.validate_artifact_layout(self.root, EXPECTED["artifacts"]))
        (self.root / "private-receipt.json").unlink()
        (self.root / "sdr_fox.h").unlink()
        self.fails("artifact-file-set-mismatch", lambda: v.validate_artifact_layout(self.root, EXPECTED["artifacts"]))

    def test_same_size_byte_tamper_fails(self):
        a = {"role": "sample", "path": "sample", "sha256": v.sha256(b"ABCD"), "size_bytes": 4}
        (self.root / "sample").write_bytes(b"ABCE")
        self.fails("artifact-bytes-mismatch", lambda: v.check_artifact(self.root, a, []))

    def test_even_matching_bytes_fail_host_path_leak(self):
        data = b"debug /private/actual-home/file"
        (self.root / "sample").write_bytes(data)
        a = {"role": "sample", "path": "sample", "sha256": v.sha256(data), "size_bytes": len(data)}
        self.fails("host-path-in-artifact", lambda: v.check_artifact(self.root, a, [Path("/private/actual-home")]))

    def test_redirecting_environment_fails_without_echoing_value(self):
        for key in ("GIT_DIR", "GIT_CONFIG_COUNT", "CARGO_HOME", "CARGO_BUILD_RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER"):
            with self.subTest(key=key):
                self.fails("redirecting-environment", lambda: v.validate_environment(self.root, {"HOME": str(self.root), key: "secret-value"}))

    def test_arbitrary_rustflags_rejected(self):
        self.fails("unexpected-rustflags", lambda: v.validate_environment(self.root, {"HOME": str(self.root), "RUSTFLAGS": "-C target-cpu=native"}))

    def test_inherited_cargo_config_rejected(self):
        (self.root / ".cargo").mkdir()
        (self.root / ".cargo/config.toml").write_text('[source.crates-io]\nreplace-with="alternate"\n')
        self.fails("unreviewed-cargo-config", lambda: v.validate_environment(self.root, {"HOME": str(self.root)}))

    def test_malformed_and_empty_cargo_tree_rejected(self):
        for text in ("", "some warning\n", "example\n", "p v1.0\nsecret stray line\n"):
            with self.subTest(text=text):
                with self.assertRaises(v.VerificationError):
                    v.parse_tree(text)
        self.assertEqual(v.parse_tree("p v1.0\np v1.0 (*)\nworkspace v0.1.0 (/safe/source)\n"), {("p", "1.0"), ("workspace", "0.1.0")})

    def elf_outputs(self, c):
        header = "ELF Header:\n Class: ELF64\n Type: DYN (Shared object file)\n Machine: " + c["machine_description"] + "\n"
        header += "".join(" (NEEDED) Shared library: [" + dep + "]\n" for dep in c["needed"])
        symbols = "".join("00000100 T " + n + "\n" for n in c["defined_dynamic_exports"])
        return header, symbols

    def test_real_inspection_is_required_for_elf(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        with patch.object(v, "run", side_effect=self.elf_outputs(c)) as commands:
            result = v.inspect_elf(synthetic_elf(), self.root / "sample.so", c, self.root)
        self.assertEqual(result["defined_export_count"], 18)
        self.assertEqual(commands.call_count, 2)
        self.assertTrue(commands.call_args_list[0].args[0][0].endswith("llvm-readelf"))
        self.assertTrue(commands.call_args_list[1].args[0][0].endswith("llvm-nm"))

    def test_elf_wrong_architecture_rejected_before_inspector(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        with patch.object(v, "run") as commands:
            self.fails("elf-architecture-mismatch", lambda: v.inspect_elf(synthetic_elf(62), self.root / "sample.so", c, self.root))
        commands.assert_not_called()

    def test_elf_rejects_forbidden_libusb_bytes(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        self.fails("forbidden-elf-content", lambda: v.inspect_elf(synthetic_elf() + b"LiBuSb", self.root / "sample.so", c, self.root))

    def test_elf_extra_dynamic_dependency_rejected(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        header, _ = self.elf_outputs(c)
        with patch.object(v, "run", return_value=header + " (NEEDED) Shared library: [libextra.so]\n"):
            self.fails("elf-needed-mismatch", lambda: v.inspect_elf(synthetic_elf(), self.root / "sample.so", c, self.root))

    def test_elf_empty_dynamic_section_not_a_pass(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        header = "Class: ELF64\n Type: DYN\n Machine: AArch64\n"
        with patch.object(v, "run", return_value=header):
            self.fails("elf-needed-mismatch", lambda: v.inspect_elf(synthetic_elf(), self.root / "sample.so", c, self.root))

    def test_elf_extra_export_and_missing_export_rejected(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        header, symbols = self.elf_outputs(c)
        for changed in (symbols + "00000200 T unexpected_export\n", "\n".join(symbols.splitlines()[1:])):
            with self.subTest(changed=changed[-30:]), patch.object(v, "run", side_effect=(header, changed)):
                self.fails("elf-exports-mismatch", lambda: v.inspect_elf(synthetic_elf(), self.root / "sample.so", c, self.root))

    def test_elf_rpath_rejected(self):
        c = EXPECTED["abi"]["elf_contracts"][0]
        header, _ = self.elf_outputs(c)
        with patch.object(v, "run", return_value=header + " (RUNPATH) /untrusted\n"):
            self.fails("elf-runtime-path", lambda: v.inspect_elf(synthetic_elf(), self.root / "sample.so", c, self.root))

    def test_macho_member_architecture_and_truncation_rejected(self):
        self.assertEqual(v.parse_macho_archive(synthetic_archive()), (1, {"14.0": 1}))
        self.fails("macho-member-architecture-mismatch", lambda: v.parse_macho_archive(synthetic_archive(cpu=16777223)))
        self.fails("truncated-archive-member", lambda: v.parse_macho_archive(synthetic_archive()[:-1]))
        self.fails("empty-or-truncated-archive", lambda: v.parse_macho_archive(b"!<arch>\n"))

    def test_macho_missing_build_version_rejected(self):
        data = bytearray(synthetic_archive())
        struct.pack_into("<I", data, 8 + 60 + 32, 0x2)
        self.fails("macho-build-version-missing", lambda: v.parse_macho_archive(bytes(data)))

    def test_macho_dynamic_dependency_in_object_rejected(self):
        data = bytearray(synthetic_archive())
        struct.pack_into("<I", data, 8 + 60 + 32, 0xC)
        self.fails("dynamic-dependency-in-static-archive", lambda: v.parse_macho_archive(bytes(data)))

    def test_macho_inspector_error_cannot_be_empty_symbol_pass(self):
        c = copy.deepcopy(EXPECTED["abi"]["c"]["macos_archive"])
        c.update(observed_macho_member_count=1, minimum_versions={"14.0": 1})
        with patch.object(v, "run", side_effect=("arm64", v.VerificationError("inspection-command-failed"))):
            self.fails("inspection-command-failed", lambda: v.inspect_macos(synthetic_archive(), self.root / "sample.a", c))

    def test_macho_otool_must_agree_with_raw_structure(self):
        c = copy.deepcopy(EXPECTED["abi"]["c"]["macos_archive"])
        c.update(observed_macho_member_count=1, minimum_versions={"14.0": 1})
        symbols = "".join("00000000 T " + name + "\n" for name in c["c_abi_defined_exports"])
        with patch.object(v, "run", side_effect=("arm64", symbols, "minos 11.0\nplatform 1\n")):
            self.fails("otool-minimum-versions-mismatch", lambda: v.inspect_macos(synthetic_archive(), self.root / "sample.a", c))

    def test_failed_check_never_becomes_pass_and_no_raw_error_output(self):
        report = v.Report()
        report.check("inspection", lambda: v.require(False, "inspection-command-failed"))
        report.check("later", lambda: {"count": 1})
        result = report.result(EXPECTED)
        self.assertEqual(result["status"], "fail")
        self.assertFalse(result["fresh_build_proven"])
        self.assertNotIn(str(self.root), json.dumps(result))

    def test_failed_source_prevents_any_cargo_or_ndk_execution(self):
        with patch.object(v, "validate_environment", return_value=(self.root, self.root / ".cargo")), patch.object(v, "check_source", side_effect=v.VerificationError("source-commit-mismatch")), patch.object(v, "check_toolchain") as toolchain, patch.object(v, "verify_packages") as packages:
            result = v.verify(self.root, self.root / "artifacts", self.root / "ndk", EXPECTED)
        self.assertEqual(result["status"], "fail")
        toolchain.assert_not_called()
        packages.assert_not_called()

    def test_report_cannot_overwrite_source_or_artifacts(self):
        self.fails("report-overwrites-input", lambda: v.write_report(self.root / "source/result.json", {}, self.root / "source", self.root / "artifacts"))
        self.fails("report-overwrites-input", lambda: v.write_report(self.root / "artifacts/result.json", {}, self.root / "source", self.root / "artifacts"))

    def test_atomic_report_is_private_and_sanitized(self):
        output = self.root / "evidence/result.json"
        v.write_report(output, {"status": "fail"}, self.root / "source", self.root / "artifacts")
        self.assertEqual(output.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(output.read_text()), {"status": "fail"})

    def build_receipt_fixture(self):
        receipt = {"schema": 1, "status": "built_pending_independent_verification",
                   "source_commit": EXPECTED["source"]["commit"], "source_tree": EXPECTED["source"]["tree"],
                   "source_root": EXPECTED["source"]["verified_fresh_history_root"],
                   "lock_sha256": EXPECTED["source"]["cargo_lock_sha256"],
                   "artifacts": [{"path": "artifacts/" + a["path"], "sha256": a["sha256"], "size_bytes": a["size_bytes"]}
                                 for a in EXPECTED["artifacts"]],
                   "commands": [{"label": "smoke-run", "status": "pass", "exit_code": 0,
                                 "argv": ["/synthetic/smoke"], "log": "logs/01-smoke-run.log", "log_sha256": v.sha256(b"0.1.0\n")}]}
        (self.root / "logs").mkdir()
        (self.root / "logs/01-smoke-run.log").write_bytes(b"0.1.0\n")
        path = self.root / "build-receipt.json"
        path.write_text(json.dumps(receipt))
        return receipt, path

    def test_build_receipt_requires_success_exact_source_and_all_artifact_identities(self):
        receipt, path = self.build_receipt_fixture()
        v.read_build_receipt(path, self.root / "artifacts", EXPECTED)
        cases = [receipt | {"status": "failed"}, receipt | {"source_commit": "0" * 40},
                 receipt | {"artifacts": receipt["artifacts"][:-1]},
                 receipt | {"commands": [receipt["commands"][0] | {"exit_code": 1}]}]
        for changed in cases:
            path.write_text(json.dumps(changed))
            with self.subTest(fields=list(changed)), self.assertRaises(v.VerificationError):
                v.read_build_receipt(path, self.root / "artifacts", EXPECTED)

    def test_duplicate_build_receipt_key_and_artifact_cannot_hide_failure(self):
        receipt, path = self.build_receipt_fixture()
        path.write_text('{"status":"failed","status":"built_pending_independent_verification"}')
        self.fails("duplicate-build-receipt-key", lambda: v.read_build_receipt(path, self.root / "artifacts", EXPECTED))
        receipt["artifacts"][-1] = receipt["artifacts"][0]
        path.write_text(json.dumps(receipt))
        self.fails("duplicate-or-invalid-build-artifact", lambda: v.read_build_receipt(path, self.root / "artifacts", EXPECTED))

    def test_measured_log_requires_success_unique_command_and_unchanged_bytes(self):
        receipt, _ = self.build_receipt_fixture()
        self.assertEqual(v.checked_command(receipt, self.root, "smoke-run")[1], "0.1.0\n")
        (self.root / "logs/01-smoke-run.log").write_bytes(b"0.2.0\n")
        self.fails("measured-command-log-hash-mismatch", lambda: v.checked_command(receipt, self.root, "smoke-run"))
        receipt["commands"].append(receipt["commands"][0])
        self.fails("measured-command-missing-or-ambiguous", lambda: v.checked_command(receipt, self.root, "smoke-run"))

    def test_measured_log_cannot_escape_or_pass_a_failed_command(self):
        receipt, _ = self.build_receipt_fixture()
        receipt["commands"][0]["status"] = "failed"
        self.fails("measured-command-failed", lambda: v.checked_command(receipt, self.root, "smoke-run"))
        receipt["commands"][0]["status"] = "pass"
        receipt["commands"][0]["log"] = "../private.log"
        self.fails("measured-command-log-path-mismatch", lambda: v.checked_command(receipt, self.root, "smoke-run"))

    def measured_fixture(self):
        expected = copy.deepcopy(EXPECTED)
        evidence = self.root / "evidence"
        artifacts = evidence / "artifacts"
        (evidence / "logs").mkdir(parents=True)
        roles = {"$SOURCE": self.root / "source", "$HOME": self.root / "home", "$TOOL_ROOT": self.root / "tools",
                 "$RUSTUP_HOME": self.root / "rustup", "$NDK_HOME": self.root / "ndk", "$SCRATCH": self.root / "scratch",
                 "$RUNNER_TEMP": self.root, "$DEVELOPER_DIR": self.root / "developer",
                 "$ANDROID_SDK_ROOT": self.root / "sdk", "$TMPDIR": self.root / "tmp"}
        for path in roles.values():
            path.mkdir(exist_ok=True)
        for a in expected["artifacts"]:
            path = artifacts / a["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            data = ("synthetic artifact " + a["role"]).encode()
            path.write_bytes(data)
            a.update(sha256=v.sha256(data), size_bytes=len(data))
        receipt = {"schema": 1, "status": "built_pending_independent_verification",
                   "source_commit": expected["source"]["commit"], "source_tree": expected["source"]["tree"],
                   "source_root": expected["source"]["verified_fresh_history_root"], "lock_sha256": expected["source"]["cargo_lock_sha256"],
                   "artifacts": [{"path": "artifacts/" + a["path"], "sha256": a["sha256"], "size_bytes": a["size_bytes"]} for a in expected["artifacts"]],
                   "commands": []}
        def command(label, argv, output):
            path = "logs/" + f"{len(receipt['commands']) + 1:02d}" + "-" + label + ".log"
            data = output.encode()
            (evidence / path).write_bytes(data)
            ref = {"path": path, "sha256": v.sha256(data)}
            receipt["commands"].append({"label": label, "status": "pass", "exit_code": 0,
                "argv": [v.normalized_paths(arg, roles) for arg in argv], "log": path, "log_sha256": ref["sha256"]})
            return ref
        apple = {"darwin": {"system": "Darwin", "release": "25.0.0", "version": "Darwin Kernel Version 25.0.0: synthetic", "machine": "arm64"}}
        observed = {"darwin": copy.deepcopy(apple["darwin"]) | {"logs": {}}}
        for name, flag in (("system", "-s"), ("release", "-r"), ("version", "-v"), ("machine", "-m")):
            observed["darwin"]["logs"][name] = command("darwin-" + name, ["uname", flag], apple["darwin"][name] + "\n")
        for tool, version in (("clang", "Apple clang version 20.0.0"), ("nm", "llvm-nm, compatible with GNU nm\nApple LLVM version 20.0.0")):
            path = roles["$DEVELOPER_DIR"] / tool
            path.write_bytes(("synthetic " + tool).encode())
            identity = {"path_role": "apple-" + tool, "path": "$DEVELOPER_DIR/" + tool, "sha256": v.sha256(path.read_bytes()), "version": version}
            apple["apple_" + tool] = identity
            observed["apple_" + tool] = identity | {
                "path_log": command("apple-" + tool + "-path", ["xcrun", "--find", tool], str(path) + "\n"),
                "version_log": command("apple-" + tool + "-version", [str(path), "--version"], version + "\n")}
        receipt["observed_tools"] = observed
        cargo_ndk = roles["$TOOL_ROOT"] / "bin/cargo-ndk"
        cargo_ndk.parent.mkdir()
        cargo_ndk.write_bytes(b"synthetic cargo-ndk")
        receipt["cargo_ndk_executable_sha256"] = v.sha256(cargo_ndk.read_bytes())
        sdk = roles["$DEVELOPER_DIR"] / "sdk"
        sdk.mkdir()
        smoke_source = roles["$SCRATCH"] / "sdr-version-smoke.c"
        smoke_source.write_text('#include "sdr_fox.h"\n#include <stdio.h>\n#include <string.h>\nint main(void) {\n    const char *version = sdrfox_version();\n    if (version == NULL || strcmp(version, "0.1.0") != 0) return 1;\n    return puts(version) < 0 ? 2 : 0;\n}\n')
        executable = roles["$SCRATCH"] / "sdr-version-smoke"
        executable.write_bytes(b"synthetic executable; execution is mocked")
        executable.chmod(0o700)
        by_role = {a["role"]: a for a in expected["artifacts"]}
        smoke = {"status": "pass", "expected_version": "0.1.0", "version": "0.1.0", "link_exit_code": 0, "run_exit_code": 0,
                 "source_path": "$SCRATCH/sdr-version-smoke.c", "source_sha256": v.sha256(smoke_source.read_bytes()),
                 "executable_path": "$SCRATCH/sdr-version-smoke", "executable_sha256": v.sha256(executable.read_bytes()),
                 "header_sha256": by_role["c_header"]["sha256"], "archive_sha256": by_role["macos_archive"]["sha256"],
                 "stdout_sha256": v.sha256(b"0.1.0\n"), "sdk_path": "$DEVELOPER_DIR/sdk"}
        smoke["sdk_path_log"] = command("apple-sdk-path", ["xcrun", "--sdk", "macosx", "--show-sdk-path"], str(sdk) + "\n")
        smoke["link_log"] = command("macos-c-link-smoke", [str(roles["$DEVELOPER_DIR"] / "clang"), "-std=c11", "-Wall", "-Wextra", "-Werror", "-arch", "arm64",
            "-mmacosx-version-min=14.0", "-isysroot", str(sdk), "-I", str(artifacts), str(smoke_source), str(artifacts / "libsdr_fox_ffi.a"),
            "-framework", "IOKit", "-framework", "CoreFoundation", "-liconv", "-lSystem", "-o", str(executable)], "")
        smoke["run_log"] = command("macos-c-run-smoke", [str(executable)], "0.1.0\n")
        receipt["c_link_smoke"] = smoke
        ndk_bin = roles["$NDK_HOME"] / "toolchains/llvm/prebuilt/darwin-x86_64/bin"
        ndk_bin.mkdir(parents=True)
        (ndk_bin / "clang-18").write_bytes(b"synthetic clang")
        (ndk_bin / "clang").symlink_to("clang-18")
        (ndk_bin / "ld.lld").write_bytes(b"synthetic linker")
        rows, raw_build = [], ""
        for abi, target in v.ANDROID_TARGETS.items():
            wrapper = {"ANDROID_PLATFORM": "21", "ANDROID_ABI": abi,
                       "_CARGO_NDK_LINK_CLANG": str(ndk_bin / "clang"), "_CARGO_NDK_LINK_TARGET": "--target=" + target + "21",
                       "CARGO_TARGET_" + target.upper().replace("-", "_") + "_LINKER": str(cargo_ndk)}
            rustc = [str(roles["$RUSTUP_HOME"] / "toolchains/1.95.0-aarch64-apple-darwin/bin/rustc"), "--crate-name", "sdr_fox_jni", "--crate-type", "cdylib", "--crate-type", "rlib", "--target", target, "-C", "linker=" + str(cargo_ndk)]
            raw_build += "Building " + abi + " (" + target + ")\n" + "".join("Exporting " + key + "=" + json.dumps(value) + "\n" for key, value in wrapper.items())
            raw_build += "Running `RUSTC=" + rustc[0] + " " + " ".join(rustc) + "`\n"
            rows.append({"abi": abi, "rust_target": target, "api_level": 21, "clang_target": "--target=" + target + "21",
                         "clang_path": "$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/clang", "linker_path": "$TOOL_ROOT/bin/cargo-ndk",
                         "rustc_argv": [v.normalized_paths(arg, roles) for arg in rustc],
                         "wrapper_assignments": {key: v.normalized_paths(value, roles) for key, value in wrapper.items()}})
        build_ref = command("build-android", ["cargo", "ndk", "--platform", "21", "-t", "arm64-v8a", "-t", "x86_64", "build", "-vv", "--color", "never", "--locked", "--offline", "--release", "-p", "sdr-fox-jni", "--features", "android"], raw_build)
        probe = roles["$SCRATCH"] / "android-driver.c"
        probe.write_text("int main(void) { return 0; }\n")
        for row in rows:
            target = row["rust_target"]
            triple = target.replace("-linux-", "-unknown-linux-") + "21"
            library = ndk_bin.parent / "sysroot/usr/lib" / target / "21"
            printed = str(ndk_bin) + "/../sysroot/usr/lib/" + target + "/21"
            linker_argv = [str(ndk_bin / "ld.lld"), "-shared", printed + "/crtbegin_so.o", "-L" + printed, printed + "/crtend_so.o"]
            log = " ".join(json.dumps(arg) for arg in [str(ndk_bin / "clang-18"), "-cc1", "-triple", triple]) + "\n"
            log += " ".join(json.dumps(arg) for arg in linker_argv) + "\n"
            ref = command("android-clang-" + target, [str(ndk_bin / "clang"), row["clang_target"], "-###", "-shared", "-fPIC", "-x", "c", str(probe), "-o", str(roles["$SCRATCH"] / ("android-driver-" + row["abi"]))], log)
            row["build_log"] = build_ref
            row["clang_driver"] = {"cc1_triple": triple, "api_level": 21, "platform_library_path": v.normalized_paths(str(library), roles),
                "crt_objects": [v.normalized_paths(str(library / name), roles) for name in ("crtbegin_so.o", "crtend_so.o")],
                "linker_argv": [v.normalized_paths(arg, roles) for arg in linker_argv], "log": ref, "probe_source_sha256": v.sha256(probe.read_bytes())}
        receipt["android_effective_commands"] = rows
        path = evidence / "build-receipt.json"
        path.write_text(json.dumps(receipt))
        return receipt, path, artifacts, roles, expected, apple

    def smoke_command(self, roles):
        def run(argv, **kwargs):
            if argv[0] == "/usr/bin/xcrun":
                return str(roles["$DEVELOPER_DIR"] / "sdk") + "\n"
            self.assertEqual(argv, [str(roles["$SCRATCH"] / "sdr-version-smoke")])
            self.assertEqual(kwargs.get("timeout"), 15)
            return "0.1.0\n"
        return run

    def test_complete_measured_receipt_reparses_logs_and_replays_bound_smoke(self):
        receipt, path, artifacts, roles, expected, apple = self.measured_fixture()
        with patch.object(v, "build_path_roles", return_value=roles), patch.object(v, "observed_apple_tools", return_value=apple), patch.object(v, "run", side_effect=self.smoke_command(roles)):
            result = v.verify_build_evidence(path, roles["$SOURCE"], artifacts, roles["$NDK_HOME"], expected)
        self.assertEqual(result["c_link_smoke"]["independent_run"], "pass")
        self.assertEqual(len(result["android_targets"]), 2)
        self.assertNotIn(str(self.root), json.dumps(result))
        report = v.Report()
        report.check("synthetic", lambda: result)
        self.assertFalse(report.result(expected)["fresh_build_proven"])

    def test_missing_smoke_failed_link_wrong_version_or_stdout_cannot_pass(self):
        receipt, path, artifacts, roles, expected, apple = self.measured_fixture()
        for changed in (None, receipt["c_link_smoke"] | {"link_exit_code": 1}, receipt["c_link_smoke"] | {"version": "0.2.0"}, receipt["c_link_smoke"] | {"stdout_sha256": "0" * 64}):
            with patch.object(v, "run", side_effect=self.smoke_command(roles)), self.assertRaises(v.VerificationError):
                v.verify_c_smoke(receipt | {"c_link_smoke": changed}, path.parent, artifacts, roles, expected, apple)

    def test_smoke_executable_tamper_rejected_before_execution(self):
        receipt, path, artifacts, roles, expected, apple = self.measured_fixture()
        (roles["$SCRATCH"] / "sdr-version-smoke").write_bytes(b"different executable")
        with patch.object(v, "run") as execute:
            self.fails("c-link-smoke-executable-mismatch", lambda: v.verify_c_smoke(receipt, path.parent, artifacts, roles, expected, apple))
        execute.assert_not_called()

    def test_independent_smoke_rerun_failure_is_not_hidden_by_success_receipt(self):
        receipt, path, artifacts, roles, expected, apple = self.measured_fixture()
        results = [str(roles["$DEVELOPER_DIR"] / "sdk") + "\n", v.VerificationError("inspection-command-failed")]
        with patch.object(v, "run", side_effect=results):
            self.fails("inspection-command-failed", lambda: v.verify_c_smoke(receipt, path.parent, artifacts, roles, expected, apple))

    def test_changed_live_apple_tool_or_kernel_rejects_old_observation(self):
        receipt, path, _, roles, _, apple = self.measured_fixture()
        for section, field, value in (("apple_clang", "sha256", "0" * 64), ("apple_nm", "version", "different"), ("darwin", "release", "26.0.0")):
            actual = copy.deepcopy(apple)
            actual[section][field] = value
            with self.subTest(section=section), self.assertRaises(v.VerificationError):
                v.verify_observed_tools(receipt, path.parent, roles, actual)
        with self.assertRaises(v.VerificationError):
            v.verify_observed_tools(receipt | {"observed_tools": {}}, path.parent, roles, apple)

    def test_android_declared_api_or_linker_cannot_override_measured_log(self):
        receipt, path, _, roles, expected, _ = self.measured_fixture()
        for key, value in (("api_level", 22), ("linker_path", "$TOOL_ROOT/bin/other"), ("rust_target", "wrong-target")):
            altered = copy.deepcopy(receipt)
            altered["android_effective_commands"][0][key] = value
            with self.subTest(key=key), self.assertRaises(v.VerificationError):
                v.verify_android_observations(altered, path.parent, roles, expected)

    def test_android_measured_wrong_api_missing_compiler_and_duplicate_abi_fail(self):
        receipt, path, _, roles, _, _ = self.measured_fixture()
        _, log = v.checked_command(receipt, path.parent, "build-android")
        for changed in (log.replace('ANDROID_PLATFORM="21"', 'ANDROID_PLATFORM="22"'), log.replace("--crate-name sdr_fox_jni", "--crate-name other"), log + log):
            with self.assertRaises(v.VerificationError):
                v.parse_android_build(changed, roles)

    def test_real_cargo_dual_crate_types_and_rustc_assignment_are_accepted(self):
        receipt, path, _, roles, _, _ = self.measured_fixture()
        _, log = v.checked_command(receipt, path.parent, "build-android")
        self.assertIn("Running `RUSTC=", log)
        actual = "--crate-type cdylib --crate-type rlib"
        for flags in (actual, "--crate-type cdylib,rlib", "--crate-type=rlib,cdylib",
                      "--crate-type=cdylib --crate-type rlib"):
            with self.subTest(flags=flags):
                rows = v.parse_android_build(log.replace(actual, flags), roles)
                self.assertEqual(len(rows), 2)
                for row in rows:
                    self.assertEqual(v.jni_crate_types(row["rustc_argv"]), {"cdylib", "rlib"})

    def test_jni_crate_type_omissions_additions_and_duplicates_are_rejected(self):
        receipt, path, _, roles, _, _ = self.measured_fixture()
        _, log = v.checked_command(receipt, path.parent, "build-android")
        actual = "--crate-type cdylib --crate-type rlib"
        for flags in ("", "--crate-type cdylib", "--crate-type rlib",
                      "--crate-type cdylib,staticlib", "--crate-type cdylib,rlib,staticlib",
                      "--crate-type cdylib,cdylib,rlib", "--crate-type cdylib,"):
            with self.subTest(flags=flags), self.assertRaises(v.VerificationError):
                v.parse_android_build(log.replace(actual, flags), roles)

    def test_a_second_actual_rustc_executable_is_rejected(self):
        receipt, path, _, roles, _, _ = self.measured_fixture()
        _, log = v.checked_command(receipt, path.parent, "build-android")
        with self.assertRaises(v.VerificationError):
            v.parse_android_build(log.replace(" --crate-name", " /unexpected/bin/rustc --crate-name"), roles)

    def test_clang_actual_api22_crt_and_unpinned_tool_are_rejected(self):
        receipt, path, _, roles, _, _ = self.measured_fixture()
        _, log = v.checked_command(receipt, path.parent, "android-clang-aarch64-linux-android")
        for changed in (log.replace("/21/", "/22/"), log.replace("android21", "android22"), log.replace("clang-18", "clang-unknown")):
            with self.assertRaises(v.VerificationError):
                v.parse_clang_driver(changed, "aarch64-linux-android", roles)

    def package_fixture(self):
        source, cargo = self.root / "source", self.root / "cargo"
        source.mkdir()
        data = b"synthetic original crate"
        checksum = v.sha256(data)
        registry = "registry+https://github.com/rust-lang/crates.io-index"
        (source / "Cargo.lock").write_text(f'[[package]]\nname="example"\nversion="1.0.0"\nsource="{registry}"\nchecksum="{checksum}"\n')
        archive = cargo / "registry/cache/index.crates.io-1234/example-1.0.0.crate"
        archive.parent.mkdir(parents=True)
        archive.write_bytes(data)
        manifest = cargo / "registry/src/index.crates.io-1234/example-1.0.0/Cargo.toml"
        p = {"name": "example", "version": "1.0.0", "cargo_source": registry, "cargo_checksum": checksum, "source": {"kind": "registry", "archive_sha256": checksum}}
        graph = {"target": "aarch64-linux-android", "root_package": "example", "features": [], "package_count": 1, "packages": [p]}
        metadata = {"packages": [{"name": "example", "version": "1.0.0", "source": registry, "manifest_path": str(manifest)}]}
        return source, cargo, archive, graph, metadata

    def test_complete_package_set_and_actual_archive_checksum(self):
        source, cargo, _, graph, metadata = self.package_fixture()
        with patch.object(v, "run", side_effect=("example v1.0.0", json.dumps(metadata))):
            result = v.verify_packages(source, cargo, [graph])
        self.assertEqual(result[0]["package_count"], 1)

    def test_registry_archive_tamper_fails(self):
        source, cargo, archive, graph, metadata = self.package_fixture()
        archive.write_bytes(b"tampered same logical package")
        with patch.object(v, "run", side_effect=("example v1.0.0", json.dumps(metadata))):
            self.fails("registry-archive-checksum-mismatch", lambda: v.verify_packages(source, cargo, [graph]))

    def test_package_addition_or_omission_fails(self):
        source, cargo, _, graph, _ = self.package_fixture()
        for tree in ("another v1.0.0", "example v1.0.0\nextra v2.0.0"):
            with self.subTest(tree=tree), patch.object(v, "run", return_value=tree):
                self.fails("package-set-mismatch", lambda: v.verify_packages(source, cargo, [graph]))

    def test_registry_substitution_fails(self):
        source, cargo, _, graph, metadata = self.package_fixture()
        metadata["packages"][0]["source"] = "registry+https://untrusted.invalid/index"
        with patch.object(v, "run", side_effect=("example v1.0.0", json.dumps(metadata))):
            self.fails("package-source-mismatch", lambda: v.verify_packages(source, cargo, [graph]))

    def test_metadata_external_path_fails(self):
        source, cargo, _, graph, metadata = self.package_fixture()
        metadata["packages"][0]["manifest_path"] = str(self.root / "elsewhere/Cargo.toml")
        with patch.object(v, "run", side_effect=("example v1.0.0", json.dumps(metadata))):
            self.fails("registry-source-path-mismatch", lambda: v.verify_packages(source, cargo, [graph]))

    def make_source(self):
        source = self.root / "source"
        source.mkdir()
        def command(*args):
            return subprocess.run(["git", "-C", str(source), *args], capture_output=True, text=True, check=True).stdout.strip()
        command("init", "-b", "master")
        command("config", "user.name", "Synthetic Test")
        command("config", "user.email", "synthetic@example.invalid")
        command("remote", "add", "origin", "https://github.com/SondeFox/sdr-fox.git")
        (source / "Cargo.lock").write_text("version = 4\n")
        command("add", "Cargo.lock")
        command("commit", "-m", "Synthetic source identity")
        commit = command("rev-parse", "HEAD")
        expected = {"commit": commit, "tree": command("rev-parse", "HEAD^{tree}"), "verified_fresh_history_root": commit, "repository": "https://github.com/SondeFox/sdr-fox.git", "cargo_lock_sha256": v.sha256((source / "Cargo.lock").read_bytes())}
        return source, expected, command

    def test_real_git_source_identity_and_mutation(self):
        source, expected, _ = self.make_source()
        self.assertEqual(v.check_source(source, expected)["commit"], expected["commit"])
        (source / "Cargo.lock").write_text("version = 3\n")
        self.fails("modified-source", lambda: v.check_source(source, expected))

    def test_assume_unchanged_cannot_hide_source_tamper(self):
        source, expected, command = self.make_source()
        command("update-index", "--assume-unchanged", "Cargo.lock")
        (source / "Cargo.lock").write_text("version = 3\n")
        self.fails("hidden-source-state", lambda: v.check_source(source, expected))

    def test_canonical_https_spellings_accept_all_manifest_fetch_push_combinations(self):
        source, expected, command = self.make_source()
        forms = ("https://github.com/SondeFox/sdr-fox", "https://github.com/SondeFox/sdr-fox.git")
        for repository, fetch_url, push_url in itertools.product(forms, repeat=3):
            with self.subTest(repository=repository, fetch=fetch_url, push=push_url):
                command("remote", "set-url", "origin", fetch_url)
                command("remote", "set-url", "--push", "origin", push_url)
                result = v.check_source(source, expected | {"repository": repository})
                self.assertEqual(result["commit"], expected["commit"])
                self.assertEqual(result["tree"], expected["tree"])

    def test_noncanonical_spellings_fail_for_manifest_fetch_and_push(self):
        source, expected, command = self.make_source()
        canonical = "https://github.com/SondeFox/sdr-fox.git"
        rejected = (
            "https://github.com/Example/sdr-fox.git",
            "https://github.com/SondeFox/other.git",
            "https://example.invalid/SondeFox/sdr-fox.git",
            "https://github.com.example.invalid/SondeFox/sdr-fox.git",
            "https://synthetic-user@github.com/SondeFox/sdr-fox.git",
            "https://synthetic-user:synthetic-password@github.com/SondeFox/sdr-fox.git",
            canonical + "?ref=master", canonical + "#master", canonical + "/extra",
            "https://github.com/SondeFox/sdr-fox/extra", canonical + "/",
            "https://github.com:443/SondeFox/sdr-fox.git",
            "http://github.com/SondeFox/sdr-fox.git",
            "ssh://git@github.com/SondeFox/sdr-fox.git",
            "git@github.com:SondeFox/sdr-fox.git",
            " " + canonical, canonical + " ", "\t" + canonical, canonical + "\t",
            "\n" + canonical, canonical + "\n",
        )
        for location, url in itertools.product(("manifest", "fetch", "push"), rejected):
            with self.subTest(location=location, url=url):
                command("config", "--replace-all", "remote.origin.url", canonical)
                command("config", "--replace-all", "remote.origin.pushurl", canonical)
                checked = expected
                if location == "manifest":
                    checked = expected | {"repository": url}
                    code = "noncanonical-source-repository"
                else:
                    key = "remote.origin.url" if location == "fetch" else "remote.origin.pushurl"
                    command("config", "--replace-all", key, url)
                    code = "noncanonical-source-url"
                self.fails(code, lambda: v.check_source(source, checked))

    def test_additional_canonical_fetch_or_push_urls_are_rejected(self):
        source, expected, command = self.make_source()
        canonical = "https://github.com/SondeFox/sdr-fox.git"
        for key, extra in itertools.product(
                ("remote.origin.url", "remote.origin.pushurl"),
                (canonical, "https://github.com/SondeFox/sdr-fox")):
            with self.subTest(key=key, extra=extra):
                command("config", "--replace-all", "remote.origin.url", canonical)
                command("config", "--replace-all", "remote.origin.pushurl", canonical)
                command("config", "--add", key, extra)
                self.fails("noncanonical-source-url", lambda: v.check_source(source, expected))

    def test_an_additional_remote_is_rejected_even_when_canonical(self):
        source, expected, command = self.make_source()
        command("remote", "add", "extra", "https://github.com/SondeFox/sdr-fox")
        self.fails("unexpected-source-remote", lambda: v.check_source(source, expected))

    def test_canonical_alias_does_not_relax_source_identity_or_cleanliness(self):
        source, expected, command = self.make_source()
        command("remote", "set-url", "origin", "https://github.com/SondeFox/sdr-fox")
        for key, code in (("commit", "source-commit-mismatch"), ("tree", "source-tree-mismatch"),
                          ("verified_fresh_history_root", "source-root-mismatch"),
                          ("cargo_lock_sha256", "source-lock-mismatch")):
            with self.subTest(key=key):
                self.fails(code, lambda: v.check_source(source, expected | {key: "0" * 64}))
        (source / "Cargo.lock").write_text("version = 3\n")
        self.fails("modified-source", lambda: v.check_source(source, expected))

    def test_noncanonical_origin_and_wrong_commit_rejected(self):
        source, expected, command = self.make_source()
        command("remote", "set-url", "origin", "https://example.invalid/not-the-source.git")
        self.fails("noncanonical-source-url", lambda: v.check_source(source, expected))
        wrong = expected | {"commit": "0" * 40}
        self.fails("source-commit-mismatch", lambda: v.check_source(source, wrong))

    def test_committed_manifest_contains_no_private_transcripts(self):
        self.assertNotIn("input_verification", EXPECTED)
        raw = json.dumps(EXPECTED)
        for value in ("/Users/", "/private/tmp/", "/Volumes/", "discrepancies", "observations"):
            self.assertNotIn(value, raw)
        self.assertEqual(len(EXPECTED["artifacts"]), 5)
        self.assertEqual([g["package_count"] for g in EXPECTED["package_graphs"]], [73, 54, 54])
        self.assertEqual(EXPECTED["source"]["commit"], "fb34d8c600725b54c5a950234c892a593c343968")


if __name__ == "__main__":
    unittest.main()
