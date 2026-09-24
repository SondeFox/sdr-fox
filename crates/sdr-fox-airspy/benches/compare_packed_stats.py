#!/usr/bin/env python3
"""Compile a hash-pinned canonical-source oracle and candidate without dependencies.

Only local canonical Git objects are read. Temporary source copies retain their
MIT OR Apache-2.0 license; no reference implementation is vendored into Git.
This is the incremental c89 oracle; compare_cf32.py retains the historical c927 oracle.
See docs/MACOS_AIRSPY_PACKED_STATS_CPU.md for provenance and measurement limitations.
"""
import argparse
import hashlib
import json
import pathlib
import subprocess
import tempfile
import time

BASE = 'c89f580c369b86a874e24669f86779eccb76e9c3'
SOURCE = 'crates/sdr-fox-airspy/src/iq_synth.rs'
SHA256 = '8bd9aab704cb75e1fe931af86c5f128401e9723ebcac5505f3bd21fdec258fcc'
HISTORICAL_BASE = 'c927ba987f2b9e1da7a50f1d9debca92d9207447'
HISTORICAL_SHA256 = 'b8cf6460b9a5b41ed05f37de4421c67635500297e09eee80c7fa1dd24f3bc25f'
ROOT = pathlib.Path(__file__).resolve().parents[3]
SUPPORT = pathlib.Path(__file__).resolve().parent / 'support'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--build-only', action='store_true')
    parser.add_argument('--pairs', type=int, default=7)
    parser.add_argument('--iterations', type=int, default=100)
    parser.add_argument('--output-dir', type=pathlib.Path)
    parser.add_argument('--historical', action='store_true', help='Retain exact historical c927 comparison')
    parser.add_argument('--reference-source', type=pathlib.Path, help='Approved own-candidate snapshot for paired B versus A')
    parser.add_argument('--reference-sha256')
    parser.add_argument('--candidate-source', type=pathlib.Path, help='Approved own-candidate snapshot for calibration or A')
    parser.add_argument('--candidate-sha256')
    args = parser.parse_args()
    if args.pairs < 1 or args.iterations < 1:
        parser.error('--pairs and --iterations must be positive')
    origin = subprocess.check_output(['git', 'remote', 'get-url', 'origin'], cwd=ROOT, text=True).strip()
    if origin.removesuffix('.git') != 'https://github.com/SondeFox/sdr-fox':
        raise SystemExit('Refusing noncanonical origin')
    base, baseline_hash = (HISTORICAL_BASE, HISTORICAL_SHA256) if args.historical else (BASE, SHA256)
    source = subprocess.check_output(['git', 'show', f'{base}:{SOURCE}'], cwd=ROOT)
    if hashlib.sha256(source).hexdigest() != baseline_hash:
        raise SystemExit('Canonical baseline source hash mismatch')
    def snapshot(path, expected, label):
        if (path is None) != (expected is None):
            parser.error(f'{label} snapshot path and SHA-256 are both required')
        if path is None:
            return None
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != expected:
            raise SystemExit(f'{label} snapshot hash mismatch')
        return data
    reference_snapshot = snapshot(args.reference_source, args.reference_sha256, 'reference')
    if reference_snapshot is not None:
        source = reference_snapshot
    current = (ROOT / SOURCE).read_bytes()
    candidate_snapshot = snapshot(args.candidate_source, args.candidate_sha256, 'candidate')
    if candidate_snapshot is not None:
        current = candidate_snapshot
    out = args.output_dir or pathlib.Path(tempfile.mkdtemp(prefix='airspy-cf32-'))
    out.mkdir(parents=True, exist_ok=True)
    shim = (SUPPORT / 'oracle_shim.rs').read_bytes()
    for name, data in [('reference', source), ('candidate', current)]:
        (out / f'{name}.rs').write_bytes(data + b'\n' + shim)
    (out / 'compare.rs').write_bytes((SUPPORT / 'compare.rs').read_bytes())
    command = ['rustc', '--edition=2021', '-C', 'opt-level=3', '-C', 'lto=fat', '-C',
               'codegen-units=1', '-C', 'strip=symbols', '-A', 'dead_code',
               f'--emit=link={out / "compare"},asm={out / "compare.s"}', str(out / 'compare.rs')]
    start = time.monotonic()
    subprocess.run(command, cwd=ROOT, check=True)
    metadata = {'baseline_commit': base, 'baseline_sha256': baseline_hash,
                'reference_commit': base if reference_snapshot is None else None,
                'reference_source': str(args.reference_source) if args.reference_source else None,
                'reference_sha256': hashlib.sha256(source).hexdigest(),
                'candidate_source': str(args.candidate_source or (ROOT / SOURCE)),
                'candidate_sha256': hashlib.sha256(current).hexdigest(),
                'support_sha256': {name: hashlib.sha256((SUPPORT / name).read_bytes()).hexdigest()
                                   for name in ['compare.rs', 'oracle_shim.rs']},
                'harness_sha256': hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
                'origin': origin, 'cwd': str(ROOT), 'compile_argv': command,
                'compile_seconds': time.monotonic() - start,
                'rustc': subprocess.check_output(['rustc', '-Vv'], text=True),
                'binary_sha256': hashlib.sha256((out / 'compare').read_bytes()).hexdigest(),
                'assembly_sha256': hashlib.sha256((out / 'compare.s').read_bytes()).hexdigest(),
                'argv': [str(out / 'compare'), str(args.pairs), str(args.iterations)]}
    if args.verify_only:
        metadata['argv'].append('--verify-only')
    (out / 'build.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(json.dumps({'output_dir': str(out), **metadata}), flush=True)
    if not args.build_only:
        result = subprocess.run(metadata['argv'], cwd=ROOT, capture_output=True, text=True)
        (out / 'run.log').write_text(result.stdout + result.stderr)
        print(result.stdout, end='')
        print(result.stderr, end='')
        result.check_returncode()


if __name__ == '__main__':
    main()
