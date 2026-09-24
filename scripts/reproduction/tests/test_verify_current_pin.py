"""Synthetic verifier failures; no captured/private bytes or native compilation."""
import copy
import importlib.util
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
