"""Synthetic object/archive attacks reach the classifier, never a hash shortcut."""
import contextlib
import importlib.util
import os
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'verify_current_pin.py'
spec = importlib.util.spec_from_file_location('provenance_verifier', SCRIPT)
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)
HOME = Path('/Users/runner')


def object_bytes(text=b'/Users/runner/upstream-build/source.S\0'):
    return struct.pack('<8I', 0xFEEDFACF, 16777228, 0, 1, 0, 0, 0, 0) + text


def member(name, payload, extended=False):
    if extended or len(name) > 15:
        body = name.encode() + payload
        field = '#1/' + str(len(name))
    else:
        body = payload
        field = name if name == '/' else name + '/'
    header = field.encode().ljust(16) + b'0'.ljust(12) + b'0'.ljust(6) + b'0'.ljust(6) + b'644'.ljust(8) + str(len(body)).encode().ljust(10) + b'`\n'
    return header + body + (b'\n' if len(body) % 2 else b'')


def archive(*members):
    return b'!<arch>\n' + b''.join(members)


class CompilerPathProvenanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        self.addCleanup(self.temp.cleanup)
        self.payload = object_bytes()
        self.reference = archive(member('lib.rmeta', b'synthetic metadata'), member('known.o', self.payload))

    @contextlib.contextmanager
    def pinned_reference(self, reference=None):
        reference = self.reference if reference is None else reference
        with patch.object(v, 'COMPILER_BUILTINS_SHA256', v.sha256(reference)), patch.object(v, 'COMPILER_BUILTINS_SIZE', len(reference)):
            yield reference

    def classify(self, candidate, reference=None):
        return v.classify_compiler_home_paths(candidate, HOME, self.reference if reference is None else reference)

    def test_exact_payload_name_length_hash_classifies_and_counts(self):
        candidate = archive(member('known.o', self.payload, extended=True))
        with self.pinned_reference():
            result = self.classify(candidate)
        self.assertEqual(result['inherited_compiler_path_members'], 1)
        self.assertEqual(result['inherited_compiler_path_occurrences'], 1)
        self.assertEqual(result['reference_sha256'], v.sha256(self.reference))
        self.assertNotIn(str(HOME), str(result))

    def test_one_modified_byte_or_spoofed_name_fails_after_valid_archive_parse(self):
        candidates = [archive(member('known.o', self.payload[:-1] + b'X')),
                      archive(member('spoof.o', self.payload)),
                      archive(member('known.o', self.payload + b'extra'))]
        with self.pinned_reference():
            for candidate in candidates:
                with self.subTest(size=len(candidate)), self.assertRaisesRegex(v.VerificationError, 'unproved-or-extra-host-path-object'):
                    self.classify(candidate)

    def test_duplicate_object_multiplicity_is_not_a_set_membership_pass(self):
        candidate = archive(member('known.o', self.payload), member('known.o', self.payload))
        with self.pinned_reference(), self.assertRaisesRegex(v.VerificationError, 'unproved-or-extra-host-path-object'):
            self.classify(candidate)
        reference = candidate
        with self.pinned_reference(reference):
            self.assertEqual(self.classify(candidate, reference)['inherited_compiler_path_members'], 2)
            with self.assertRaises(v.VerificationError):
                self.classify(candidate + member('known.o', self.payload), reference)

    def test_missing_or_changed_reference_fails_even_without_home_hit(self):
        candidate = archive(member('clean.o', object_bytes(b'clean')))
        with self.pinned_reference():
            for ref in (None, b'', self.reference[:-1] + b'X'):
                with self.subTest(missing=ref is None), self.assertRaisesRegex(v.VerificationError, 'compiler-reference-bytes-mismatch'):
                    v.classify_compiler_home_paths(candidate, HOME, ref)

    def test_member_not_present_in_pinned_reference_is_rejected(self):
        reference = archive(member('other.o', object_bytes(b'no home')))
        with self.pinned_reference(reference), self.assertRaisesRegex(v.VerificationError, 'unproved-or-extra-host-path-object'):
            self.classify(archive(member('known.o', self.payload)), reference)

    def test_home_in_symbol_table_or_member_name_is_not_object_provenance(self):
        candidates = [archive(member('/', b'/Users/runner/private')),
                      archive(member('/Users/runner/private.o', self.payload, extended=True))]
        with self.pinned_reference():
            for candidate in candidates:
                with self.assertRaises(v.VerificationError):
                    self.classify(candidate)

    def test_home_spanning_payload_and_next_header_is_rejected(self):
        # The even-sized payload ends '/Users/'; the next header starts 'runner'.
        partial = object_bytes(b'X/Users/')
        self.assertEqual(len(partial) % 2, 0)
        candidate = archive(member('partial.o', partial), member('runner.o', object_bytes(b'clean')))
        self.assertEqual(candidate.count(b'/Users/runner'), 1)
        with self.pinned_reference(), self.assertRaisesRegex(v.VerificationError, 'host-path-outside-proved-payload'):
            self.classify(candidate)

    def test_gnu_long_name_table_is_rejected_before_slash_normalization(self):
        entry = bytearray(member('/', b'name.o/\n'))
        entry[:16] = b'//'.ljust(16)
        with self.assertRaisesRegex(v.VerificationError, 'unsupported-provenance-long-name-table'):
            list(v.strict_ar_members(archive(bytes(entry))))

    def test_malformed_archive_numbers_names_bounds_padding_and_trailing_data_fail(self):
        valid = archive(member('known.o', self.payload))
        numeric = bytearray(valid)
        numeric[8 + 16:8 + 28] = b'1 2'.ljust(12)
        mode = bytearray(valid)
        mode[8 + 40:8 + 48] = b'6 44'.ljust(8)
        size = bytearray(valid)
        size[8 + 48:8 + 58] = b'9999999999'
        extended = archive(member('known.o', self.payload, extended=True))
        bad_name = bytearray(extended)
        bad_name[8:8 + 16] = b'#1/999999'.ljust(16)
        odd = archive(member('known.o', self.payload + (b'X' if len(self.payload) % 2 == 0 else b'')))
        self.assertEqual(odd[-1:], b'\n')
        cases = [bytes(numeric), bytes(mode), bytes(size), bytes(bad_name), valid[:-1], valid + b'junk', odd[:-1] + b'X', b'!<thin>\n']
        for start, width in ((28, 6), (34, 6), (48, 10)):
            malformed = bytearray(valid)
            malformed[8 + start:8 + start + width] = b'1 2'.ljust(width)
            cases.append(bytes(malformed))
        with self.pinned_reference():
            for candidate in cases:
                with self.subTest(length=len(candidate)), self.assertRaises(v.VerificationError):
                    self.classify(candidate)

    def test_source_cargo_and_rustup_rejection_precedes_even_proven_object_membership(self):
        for forbidden in (Path('/Users/runner/project'), Path('/Users/runner/.cargo'), Path('/Users/runner/isolated-rustup')):
            payload = object_bytes(str(forbidden).encode() + b'/private\0')
            reference = archive(member('known.o', payload))
            path = self.root / 'candidate.a'
            path.write_bytes(reference)
            artifact = {'role': 'macos_archive', 'path': path.name, 'sha256': v.sha256(reference), 'size_bytes': len(reference)}
            with self.pinned_reference(reference), patch.object(v, 'classify_compiler_home_paths') as classifier:
                with self.assertRaisesRegex(v.VerificationError, 'host-path-in-artifact'):
                    v.check_artifact(self.root, artifact, [forbidden], home=HOME, compiler_reference=reference)
                classifier.assert_not_called()

    def test_non_mac_artifacts_never_receive_reference_exception(self):
        path = self.root / 'candidate.so'
        path.write_bytes(self.reference)
        artifact = {'role': 'android_arm64_jni', 'path': path.name, 'sha256': v.sha256(self.reference), 'size_bytes': len(self.reference)}
        with self.pinned_reference(), self.assertRaisesRegex(v.VerificationError, 'host-path-in-artifact'):
            v.check_artifact(self.root, artifact, [], home=HOME, compiler_reference=self.reference)

    def loader_fixture(self):
        rustup = self.root / 'rustup'
        root = rustup / 'toolchains' / v.COMPILER_BUILTINS_TOOLCHAIN
        compiler = root / 'bin/rustc'
        compiler.parent.mkdir(parents=True)
        compiler.write_bytes(b'synthetic compiler; never executed')
        reference = root / v.COMPILER_BUILTINS_RELATIVE_PATH
        reference.parent.mkdir(parents=True)
        reference.write_bytes(self.reference)
        outputs = [v.COMPILER_BUILTINS_RUSTC_VERSION + '\n', 'host: aarch64-apple-darwin\n', str(root) + '\n']
        return rustup, compiler, reference, outputs

    @contextlib.contextmanager
    def loader_patches(self, rustup, compiler, outputs):
        with self.pinned_reference(), patch.object(v, 'COMPILER_BUILTINS_RUSTC_SHA256', v.sha256(compiler.read_bytes())), patch.object(v.shutil, 'which', return_value=str(compiler)), patch.dict(os.environ, {'RUSTUP_HOME': str(rustup)}), patch.object(v, 'run', side_effect=outputs) as commands:
            yield commands

    def test_reference_loader_requires_exact_binary_version_host_and_sysroot(self):
        rustup, compiler, reference, outputs = self.loader_fixture()
        with self.loader_patches(rustup, compiler, outputs):
            self.assertEqual(v.load_compiler_builtins_reference(), self.reference)
        for index, replacement in ((0, 'rustc 1.94.0\n'), (1, 'host: x86_64-apple-darwin\n'), (2, str(self.root) + '\n')):
            changed = list(outputs)
            changed[index] = replacement
            with self.loader_patches(rustup, compiler, changed), self.assertRaises(v.VerificationError):
                v.load_compiler_builtins_reference()
        with self.loader_patches(rustup, compiler, outputs), patch.object(v, 'COMPILER_BUILTINS_RUSTC_SHA256', '0' * 64), self.assertRaisesRegex(v.VerificationError, 'compiler-reference-rustc-mismatch'):
            v.load_compiler_builtins_reference()

    def test_reference_loader_rejects_missing_wrong_or_symlinked_reference(self):
        rustup, compiler, reference, outputs = self.loader_fixture()
        reference.unlink()
        with self.loader_patches(rustup, compiler, outputs), self.assertRaises(v.VerificationError):
            v.load_compiler_builtins_reference()
        reference.write_bytes(self.reference[:-1] + b'X')
        with self.loader_patches(rustup, compiler, outputs), self.assertRaisesRegex(v.VerificationError, 'compiler-reference-bytes-mismatch'):
            v.load_compiler_builtins_reference()
        other = self.root / 'same-bytes.rlib'
        other.write_bytes(self.reference)
        reference.unlink()
        reference.symlink_to(other)
        with self.loader_patches(rustup, compiler, outputs), self.assertRaisesRegex(v.VerificationError, 'symlink-input'):
            v.load_compiler_builtins_reference()


if __name__ == '__main__':
    unittest.main()
