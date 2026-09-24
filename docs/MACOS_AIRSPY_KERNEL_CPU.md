# Exact-arithmetic Airspy CF32 kernel experiments

These changes target full-rate background reception without changing the sample
rate, frequency coverage, filter coefficients, mixer, DC recurrence, output
formats, clipping calibration, USB policy or scheduling priority. Component
benchmarks cannot establish the consumer application's 30% CPU goal.

## Source and licensing

The reference is canonical `https://github.com/SondeFox/sdr-fox.git`, commit
`c927ba987f2b9e1da7a50f1d9debca92d9207447`, tree
`d381387a42f126a95d568a35f6cd1267c54798f0`. Its
`crates/sdr-fox-airspy/src/iq_synth.rs` SHA-256 is
`b8cf6460b9a5b41ed05f37de4421c67635500297e09eee80c7fa1dd24f3bc25f`.
The reference remains MIT OR Apache-2.0 under the repository's existing license
and notice terms. No legacy repository, GPL implementation or research source
was consulted. No dependency or lockfile changes are involved.

`benches/compare_cf32.py` reads only this local canonical Git object, refuses a
different origin or source hash, and writes temporary source copies outside
Git. It appends the same first-party observation shim to both modules; the
reference implementation bytes are otherwise unchanged. Generated synthetic
inputs, the harness, and observation shim are independently authored here
under the same MIT OR Apache-2.0 terms. No radio capture or receiver identity
is used or distributed. The temporary sources/binaries are local verification
artifacts, not new distributed dependencies or vendored fixtures.

## Numerical and streaming contract

Each FIR output retains four separate multiplication/addition chains indexed
by coefficient modulo four, followed by `(a0 + a1) + (a2 + a3)`. FIR operations
are not changed to `mul_add`, reassociated, or compiled with fast-math. The DC
recurrence still advances each container sequentially, using explicit fused
multiply-add only on the already-selected build targets. Its ADC conversion,
sign multiplication, center multiplication and final clamps are unchanged.

The current-source oracle compares output bits and state after every call:
DC bits, mixer phase, pending byte, per-call raw/clipping counters, and both
retained histories. It exercises six output/DC variants, eight tap counts
(including both parity layouts and the minimum kernel), seven deterministic
input patterns, 13 short/irregular partitions, empty calls, reset, and larger
production-size blocks. The long cases include realistic synthetic tone,
rail-to-rail, held-rail, asymmetric duty, ramps and upper container bits.
Chunk invariance tests and mathematical/image-rejection tests remain active.
A self-consistency test alone would not prove equality with the previous
implementation, which is why the frozen source oracle is separate.

## Reproduce

Run from the repository root using the existing Rust 1.95.0 toolchain:

```sh
python3 crates/sdr-fox-airspy/benches/compare_cf32.py --verify-only
python3 crates/sdr-fox-airspy/benches/compare_cf32.py --pairs 7 --iterations 100
cargo fmt --all -- --check
cargo clippy --locked -p sdr-fox-airspy --all-targets -- -D warnings
cargo test --locked -p sdr-fox-airspy
cargo test --locked --release -p sdr-fox-airspy
cargo bench --locked -p sdr-fox-airspy --bench iq_synth --no-run
```

The paired harness uses CF32 and complete 32,768/65,536/131,072-container
blocks (64/128/256 KiB raw USB payloads), preserving the synthesizer between
iterations. Each output allocation is consumed and dropped as in the existing
API. Warm-up precedes alternating reference/candidate timing order. Source,
compiler, command, assembly and binary hashes are recorded in the temporary
output directory. `--build-only` prepares an executable for a separately
coordinated quiet timing interval. The original Criterion benchmark remains
CU8-only and must not be presented as a CF32 result.

The standalone source is compiled with Release-equivalent `opt-level=3`, fat
LTO and one codegen unit, without target-native flags or architecture-specific
DSP intrinsics. It verifies the exact kernel; final linked consumer behavior
requires the separate atomic companion integration and signed application
measurements. Timing can vary with core placement and machine load. Avoid
concurrent builds/tests and record paired raw intervals rather than only the
fastest observation.

## Preliminary experiment evidence (Apple arm64, 7 September 2026)

The identical-source calibration's candidate/reference median ratios were
1.0229, 1.0073 and 0.9983 at 32,768, 65,536 and 131,072 containers. Small
percentage differences need repeated confirmation; core placement was not
forced or measured by this harness. These are elapsed component times, not
process CPU percentages or energy measurements.

Seven alternating paired rounds of 100 blocks produced these full-CF32 median
ratios against the frozen source, under root-coordinated intervals with agent
builds/tests paused:

| Candidate | 32,768 containers | 65,536 | 131,072 |
| --- | ---: | ---: | ---: |
| Fixed 8-output FIR, initialized stages and destination slices | 1.0027 | 0.9867 | 0.9871 |
| Fixed 16-output FIR, initialized stages and destination slices | 0.9558 | 0.9709 | 0.9567 |
| Direct CF32 emission with 8-output tiles | 1.1231 | 1.1256 | 1.1208 |
| Direct CF32 emission with 16-output tiles | 1.0255 | 1.0352 | 1.0394 |

Direct emission was rejected because it increased whole-path time despite
removing intermediate memory passes. Eight-output convolution did not show a
clear improvement. Assembly confirmed the original CF32 packing already
vectorized and its routing mode branch was already hoisted. The useful
structural changes were removal of dynamic tap-window checks, steady-block
zero fill, and per-container vector capacity/length maintenance; no claim is
made that the original code lacked SIMD.

Additional 16/32/256-container normalization batches preserved output/state
bits but did not improve the simpler 16-output candidate. Their median ratios
were respectively 0.9599/0.9707/0.9618, 0.9719/0.9797/0.9878, and
1.0306/1.0224/1.0282 across the same three block sizes. The 32-container
normalizer generated SIMD conversions, demonstrating that adding vector
instructions alone does not establish a whole-path win. These variants were
also rejected. The retained implementation uses the original sequential ADC
conversion/DC operations and ordinary staged CF32 packing.

The final change keeps initialized scratch/output-stage lengths between calls,
routes into exactly sized safe destination slices, and specializes the
24-coefficient FIR with 16-output fixed windows. A focused unit test additionally
compares fixed, dynamic and scalar-tail raw bits for signed-zero/subnormal and
ordinary windows. Larger platform integration, energy, receiver loss and signed
whole-application CPU acceptance remain separate gates.
