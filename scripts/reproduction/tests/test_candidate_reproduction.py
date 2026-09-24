# SPDX-License-Identifier: MIT OR Apache-2.0
"""Inert ELF/rustc-argv regressions for the new candidate; no real native input."""
from pathlib import Path
import struct
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from page_layout import check_link_args, inspect_layout
from profiles import ANDROID_PAGE_SIZE, load_manifest
from candidate_authority import projection
import verify_current_pin as verify


def elf():
    data = bytearray(0x9000)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHIQQQIHHHHHH", data, 16, 3, 183, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    struct.pack_into("<IIQQQQQQ", data, 64, 1, 5, 0, 0, 0, 0x4000, 0x4000, 0x4000)
    struct.pack_into("<IIQQQQQQ", data, 120, 1, 6, 0x4000, 0x8000, 0, 0x5000, 0x5000, 0x4000)
    struct.pack_into("<IIQQQQQQ", data, 176, 0x6474e552, 4, 0x7af0, 0xbaf0, 0, 0x1510, 0x510, 1)
    return data


class CandidateLayoutTests(unittest.TestCase):
    def test_clipped_relro_with_larger_file_extent_is_valid(self):
        self.assertEqual(inspect_layout(elf()), {"load_segments": 2, "relro_segments": 1, "page_size_bytes": 16384})

    def test_absent_relro_valid_but_missing_load_rejected(self):
        data = elf()
        struct.pack_into("<H", data, 56, 2)
        self.assertEqual(inspect_layout(data)["relro_segments"], 0)
        for offset in (64, 120):
            struct.pack_into("<I", data, offset, 4)
        with self.assertRaisesRegex(ValueError, "Missing LOAD"):
            inspect_layout(data)

    def test_load_and_relro_bounds_alignment_and_containment(self):
        mutations = ((112, "<Q", 4096), (168, "<Q", 4096),
                     (168, "<Q", 24576), (136, "<Q", 0x8001),
                     (160, "<Q", 0x4000), (216, "<Q", 0x1510),
                     (192, "<Q", 0x1baf0),
                     (32, "<Q", 0x8fff), (56, "<H", 65535))
        for offset, fmt, value in mutations:
            data = elf()
            struct.pack_into(fmt, data, offset, value)
            with self.subTest(offset=offset, value=value), self.assertRaises(ValueError):
                inspect_layout(data)
        data = elf()
        data[232:288] = data[176:232]
        struct.pack_into("<H", data, 56, 4)
        with self.assertRaisesRegex(ValueError, "duplicate RELRO"):
            inspect_layout(data)

    def test_malformed_elf_header_and_truncated_tables(self):
        for data in (b"", elf()[:63], elf()[:200], b"BAD!" + elf()[4:], elf()[:4] + b"\x01" + elf()[5:]):
            with self.subTest(size=len(data)), self.assertRaises(ValueError):
                inspect_layout(data)

    def test_new_layout_gate_does_not_relabel_historical_artifacts(self):
        contract = load_manifest().document["abi"]["elf_contracts"][0]
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<HH", header, 16, 3, 183)
        metadata = "Class: ELF64\nType: DYN\nMachine: AArch64\n" + "".join(f"(NEEDED) Shared library: [{name}]\n" for name in contract["needed"])
        symbols = "".join(f"00000000 T {name}\n" for name in contract["defined_dynamic_exports"])
        with patch.object(verify, "run", side_effect=(metadata, symbols)):
            verify.inspect_elf(header, Path("/synthetic.so"), contract, Path("/ndk"))
        with patch.object(verify, "run") as tools, self.assertRaises(ValueError):
            verify.inspect_elf(header, Path("/synthetic.so"), contract, Path("/ndk"), require_page_layout=True)
        tools.assert_not_called()


class CandidateLinkTests(unittest.TestCase):
    def arguments(self):
        return ["rustc", "--crate-name", "sdr_fox_jni", "-C", "link-arg=" + ANDROID_PAGE_SIZE.android_link_args[0],
                "-C", "link-arg=" + ANDROID_PAGE_SIZE.android_link_args[1]]

    def test_actual_codegen_args_required_not_printed_build_directives(self):
        self.assertEqual(check_link_args(self.arguments(), ANDROID_PAGE_SIZE.android_link_args), list(ANDROID_PAGE_SIZE.android_link_args))
        for argv in (self.arguments()[:-2], self.arguments() + self.arguments()[-2:],
                     self.arguments() + ["-Clink-args=-Wl,-z,common-page-size=4096"],
                     ["cargo:rustc-link-arg-cdylib=" + flag for flag in ANDROID_PAGE_SIZE.android_link_args],
                     self.arguments() + ["-C"]):
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                check_link_args(argv, ANDROID_PAGE_SIZE.android_link_args)

    def test_joined_codegen_spelling_is_checked_for_conflicts_too(self):
        argv = ["rustc"] + ["--codegen=link-arg=" + flag for flag in ANDROID_PAGE_SIZE.android_link_args]
        self.assertEqual(check_link_args(argv, ANDROID_PAGE_SIZE.android_link_args), list(ANDROID_PAGE_SIZE.android_link_args))
        with self.assertRaises(ValueError):
            check_link_args(argv + ["-Clink-arg=-Wl,-z,max-page-size=4096"], ANDROID_PAGE_SIZE.android_link_args)

    def test_receipt_cannot_select_other_manifest_or_profile(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        for bad in ({}, {"reconstruction_profile": "current-pin", "expected_manifest_sha256": manifest.sha256},
                    {"reconstruction_profile": ANDROID_PAGE_SIZE.name, "expected_manifest_sha256": "0" * 64}):
            with self.subTest(receipt=bad), patch.object(verify, "read_build_receipt", return_value=(bad, "a" * 64)), patch.object(verify, "build_path_roles") as roles, self.assertRaisesRegex(verify.VerificationError, "build-profile-or-manifest"):
                verify.verify_build_evidence(Path("/evidence/build-receipt.json"), Path("/source"), Path("/evidence/artifacts"), Path("/ndk"), manifest.document, manifest=manifest)
            roles.assert_not_called()

    def test_candidate_xcode_receipt_must_be_actual_version_command(self):
        manifest = load_manifest(ANDROID_PAGE_SIZE)
        receipt = {"reconstruction_profile": ANDROID_PAGE_SIZE.name,
                   "expected_manifest_sha256": manifest.sha256,
                   "runner": {"GITHUB_RUN_ID": "1234", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "a" * 40},
                   "authority": projection(1234, 1, "a" * 40)}
        with patch.object(verify, "read_build_receipt", return_value=(receipt, "a" * 64)), patch.object(verify, "checked_command", return_value=(["echo", "invented"], ANDROID_PAGE_SIZE.xcode_identity)), patch.object(verify, "build_path_roles") as roles, self.assertRaisesRegex(verify.VerificationError, "build-xcode-identity"):
            verify.verify_build_evidence(Path("/evidence/build-receipt.json"), Path("/source"), Path("/evidence/artifacts"), Path("/ndk"), manifest.document, manifest=manifest)
        roles.assert_not_called()


if __name__ == "__main__":
    unittest.main()
