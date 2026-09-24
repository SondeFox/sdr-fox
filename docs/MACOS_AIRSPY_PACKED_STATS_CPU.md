# Safe packed Airspy routing and exact ADC statistics

This first-party MIT OR Apache-2.0 change starts from canonical
`https://github.com/SondeFox/sdr-fox`, commit
`c89f580c369b86a874e24669f86779eccb76e9c3`, tree
`76cdb32545e3d0ddda784c054df8879dfe4f5656`. The exact reference
`crates/sdr-fox-airspy/src/iq_synth.rs` SHA-256 is
`8bd9aab704cb75e1fe931af86c5f128401e9723ebcac5505f3bd21fdec258fcc`.
The historical c927 oracle remains available separately. No legacy, research
implementation, GPL source, new dependency, copied external implementation or
radio recording is an input. No source publication is performed here.

## Implementation and invariants

The full-pair loop assembles a safe `&[u8; 4]` slice with
`u32::from_le_bytes`, masks each container to its low twelve bits, and fills
already-sized even/odd destination slices by index. This works for unaligned
and odd-start input without pointer casts, unsafe code or architecture-specific
intrinsics. The accepted arm also counts ADC rail hits from those same masked
values, eliminating the separate raw-count pass. The count remains u64 for the
whole call; it does not assume that every caller uses a small USB block.

The existing `decode_dc` is unchanged: v0 advances the DC state before v1,
with the same target-selected fused/unfused operations. Sign multiplications,
phase order, FIR coefficients/four accumulation chains, output packing and
calibration remain unchanged. No normalization prepass or direct CF32-emission
fusion is introduced; those previous experiments remain rejected.

The caller resets per-call raw/clip statistics before its pending-empty early
return. A completed saved low-byte container is counted once, as is any
odd-phase alignment container; every pair contributes two raw containers and
the final two/three-byte suffix contributes one. A trailing single byte is
counted only by the next call that completes it. Empty calls preserve pending
byte, phase, DC and carry state while reporting zero new samples/clips. Reset
clears the same state and statistics. ADC clipping still means masked values
0..4 or4092..4095. It is distinct from SondeFox's later CF32 clipping statistic.

## Separate experiments and code generation

Arm A retained the separate raw clipping pass and changed only packed loads
and iteration. Initial element-by-element byte assembly still compiled to four
byte loads; the safe four-byte array reference instead produced one `ldr w`
with masking/bit extraction. The checked indexed loop retains a cold bounds
exit; no claim is made that every branch disappeared.

Arm B adds exact raw clipping to A. Emitted code shares the masked/centered ADC
integers with conversion, uses integer comparisons/u64 conditional increments,
and retains the two ordered DC `fsub`/`fmadd` operations per pair. Its CF32
caller no longer contains the separate vectorized raw ADC-count loop. This is
a measured dataflow change, not a claim that merely adding SIMD is beneficial.

## Current and historical oracles

`benches/compare_packed_stats.py` pins the c89 source above and records hashes
for the candidate, reference, both mutable support files and the Python
harness. `--historical` selects the separately pinned c927 source. Explicit
path-plus-SHA snapshot options support A/c89, B/A and identical-source
calibration without relabeling a candidate snapshot as a Git revision.

The shared first-party oracle checks exact output bits and DC, phase, pending
byte, raw/clip counters and both histories after each call. Its1,729,567 general
comparisons cover six format/DC modes, eight tap counts, seven synthetic
patterns and short/large partitions. Another14,700 focused comparisons cover
every16-bit container encoding in both packed positions, both filter parities,
all short split points, phase-setting prefixes, unaligned slices, empty calls
with pending input and reset. Focused unit tests independently require288 rail
hits from the exhaustive two-position encoding corpus and verify pending-empty
counter/state behavior. Tests and data generation are first-party; no external
fixture or captured data is bundled.

Build/correctness uses Rust1.95, Release-equivalent opt-level3/fat LTO/one codegen
unit, no target-native flags, isolated build/TMPDIR paths and at most two Cargo
jobs. Full repository fmt/clippy/test gates plus Release/oracle, scoped secret
and unchanged dependency/header checks are recorded in the graph worker receipt.

## Root-coordinated paired timing

One quiet interval ran calibration, A/c89, B/A and B/c89 sequentially, each with
twenty warmup blocks then seven alternating pairs of100 full CF32 blocks.
Every timed output is consumed/dropped. The app was stopped and other workers
paused. The four commands, including their repeated correctness checks, took
9.947 seconds. Raw intervals remain retained; none were removed.

| Candidate/reference median ratio |64 KiB raw|128 KiB raw|256 KiB raw|
|---|---:|---:|---:|
| Identical c89 calibration |1.0079|1.0272|0.9962|
| A / c89 |0.9932|1.0010|0.9953|
| B / A |0.9331|0.9660|0.9536|
| B / c89 |0.9613|0.9534|0.9614|

A alone has no credible improvement beyond calibration scatter. B is selected
as the bounded candidate. At the production256 KiB transfer size, full-CF32
median time changed from412.59833 to396.69250 microseconds, approximately3.86%.
B/A separately improved4.64%, so the clip-fusion result does not inherit an
unproved A-only gain. The128 KiB calibration scatter is retained. These are
component elapsed times under default scheduling, not whole-app CPU/energy
results; no faster-core placement was requested or inferred.

One coordinator lease expired during preparation when its assigned heartbeat
stopped. The worker detected the gap, stopped mutations, and reported the
post-expiry edits/builds without merging or timing them. Root issued a fresh
fencing token/attempt and persistent heartbeat; every prepared source/support/
binary/assembly hash was revalidated and all five no-build exact comparisons
rerun before the quiet timing. The final receipt retains both histories.

The separate integrator must review the frozen source, combine the other
workers, rebuild/adopt Mac/header/Kotlin/bothJNI from one revision, and obtain
root's matched full-rate loss/latency/CPU evidence. Approximately30% background
CPU remains unproved. No hardware, UI, signing or adoption action is performed
by this worker.
