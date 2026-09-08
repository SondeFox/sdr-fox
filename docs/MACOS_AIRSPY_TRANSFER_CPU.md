# macOS Airspy physical transfer experiment

This first-party MIT OR Apache-2.0 experiment starts at canonical sdr-fox
`c927ba987f2b9e1da7a50f1d9debca92d9207447`, tree
`d381387a42f126a95d568a35f6cd1267c54798f0`. No legacy, GPL implementation,
research implementation, recording, dependency update or nusb modification is
an input. The published nusb 0.2.7 archive remains SHA-256
`18ef13beb3b3a8fc16fd7aea912ebd3d45dde00a9a5b968d0742297468065845`.
Its existing macOS backend submits one ReadPipeAsync with the requested buffer
length. Larger transfers may reduce per-transfer kernel/worker overhead; they
do not reduce samplewise synthesis arithmetic or the app's read/callback rate.

## Candidate policy and ownership

The internal C-ABI policy defines three candidates. After root-operated
physical comparison, macOS Airspy selects 256 KiB and retains the synthesis
bridge. Only macOS Airspy uses the selected policy; shared StreamConfig defaults and all
other platform/receiver combinations remain unchanged. No public C/JNI API or
ABI is added.

| Raw transfer | In flight | Raw queue | CF32 bridge | Fill at 10 MS/s |
| --- | ---: | ---: | ---: | ---: |
| 64 KiB | 16 | 32 | 8 | 1.6384 ms |
| 128 KiB | 8 | 16 | 4 | 3.2768 ms |
| 256 KiB | 4 | 8 | 2 | 6.5536 ms |

The three payload budgets are respectively 1 MiB raw in flight, 2 MiB raw
delivery, and 1 MiB CF32 bridge. These budgets do not bound the entire process:
the completed/working raw block, current synthesis allocation and scratch,
blocked bridge sender, pending C-ABI block, app buffer and diagnostic counters
are separate live storage. Each full synthesized CF32 block is twice its raw
byte length. The existing C ABI owns and drains its suffix without a spill
allocation; 128 KiB app reads therefore still fill exactly 32,768 floats.
Short native blocks continue to produce exact short reads.

Block sequence/count metadata advances on accepting a native block, while byte
accounting advances on each partial read. Loss metadata persists throughout
the suffix and changes once for a subsequent discontinuity. No sample is
inserted or omitted by the adapter. Raw-domain pair-drop estimates are not
exact hardware missing-IQ counts; explicit queue loss and unknown hardware
loss must be distinguished.

The transfer worker retains nonblocking drop-newest overload handling. Stop,
endpoint-wide cancel/reap-before-clear, 50 ms reap polls, cumulative 2-second
stall recovery, bridge wakeup and receiver-OFF behavior are unchanged. Larger
synthesis batches can extend work between cancellation checks; stop latency
is a physical acceptance metric.

## Root-only physical probe

Build (this never opens a receiver):

```
cargo build --locked --release -p sdr-fox-cabi --features transfer-probe --example macos_transfer_probe
```

Only the root coordinator may run the probe, after stopping the app and every
other receiver owner. Repeat the following command with `--kib 64`, `128`,
and `256` in counterbalanced order. Settings below match the verified 10 MS/s,
404 MHz, Quiet Rural, bias-on baseline; root must reverify before running.

```
target/release/examples/macos_transfer_probe --exclusive-hardware --kib 64 --seconds 60 --frequency 404000000 --rate 10000000 --lna 140 --mixer 150 --vga 150 --lna-agc 0 --mixer-agc 0 --bias 1 --bridge 1
```

All RF parameters and `--bridge 0|1` are mandatory. This probe-only switch
compares the existing direct C-ABI receive path with the synthesis bridge;
production uses the selected 256 KiB with the bridge enabled. Device family selection
always remains Airspy, independently of bridge choice. Direct mode allocates
no CF32 bridge queue or synthesis worker, and reports `bridge_enabled: false`
and zero bridge blocks. Its raw inflight/delivery byte budgets are unchanged.
Both JSON events identify the mode. Direct receive synthesizes on the calling
thread after the existing native timed receive; this experiment does not add
an interruptible synthesis step or strengthen existing read-deadline guarantees.

There is no overall gain or PPM operation;
AGC and individual stage gain writes match the app's control order. The probe
requires exactly one Airspy One and discards sample bytes without displaying
or saving them. Output has only explicit RF controls, PID and numeric counters.
Errors use fixed descriptions, never backend descriptor text. Ordinary tests
do not invoke the hardware path.

The opt-in `start_bulk_stream_observed` Rust transport method reports successful
physical payload lengths before the raw delivery queue, including subsequently
dropped blocks. Its observer records a bounded size histogram and arrival
interval summary. Ordinary application streams execute no observer. The
histogram retains at most 128 distinct sizes and counts unbucketed completions
explicitly. Timing uses bounded base-two microsecond histograms plus exact
count/sum/max/first-arrival aggregates. The probe additionally reports actual
C read cadence, short reads, timeouts/EOF, bytes and block metadata; the native
worker provides raw queue high water, delivered bytes, dropped raw blocks,
raw-pair loss estimates, failed transfers and observed unknown-overrun events.
There is no hardware sample counter: zero observed overruns does not prove
zero RF/USB hardware sample loss. Arrival timing measures host delivery/reap,
not a hardware acquisition timestamp.

The stop metric includes C adapter stop and worker joins. USB observation may
include successful completions reaped during teardown; the receive elapsed
and C copied bytes describe the measurement loop, so they must not be combined
as if all counters froze at exactly one instant. Root wraps the process with
correctly scaled user/system CPU, syscalls and context-switch measurements.
This source/copy probe does not include Kotlin or native sonde decoding; the
eventual accepted source must also pass the signed-app full-workload comparison.

## Physical result and integration choice

Root operated six 60-second streams in order 64, 128, 256, 256, 128, 64 KiB
at the explicit settings above, using source `ed1eca4ba82e44770d02a0f0ba37882df76b57ee`
and probe SHA-256
`dba49f0d0c363e656a6fe4dd42fdbec42ada1118f05f38b7bdc79c7a9dfb3ea0`.
The corrected Mach timebase was 125/3; retained process intervals were about
51 seconds after excluding startup/teardown.

| Raw transfer | Total CPU, two runs | System CPU, two runs | Mach syscalls/s, two runs |
| --- | ---: | ---: | ---: |
| 64 KiB | 19.208%, 13.570% | 5.093%, 4.014% | 6849.7, 7286.8 |
| 128 KiB | 18.363%, 12.035% | 3.827%, 2.801% | 3568.9, 3657.8 |
| 256 KiB | 17.064%, 14.640% | 2.551%, 2.251% | 1820.9, 1822.8 |

Actual successful USB payload sizes matched each requested size. Delivered
throughput was approximately 9.996–9.999 million IQ pairs/s over the complete
measurement loop, which includes startup and undrained queue boundaries.
Observed software queue drops, failed transfers, read errors, timeouts and
unknown-overrun events were zero; this is not hardware sample-loss proof.
Stop and joins took 0.968–2.964 ms.

Root then ran four 30-second direct/bridge diagnostics at source
`98addff8c5b2395d9117f0208da05efbed10a339`, probe SHA-256
`02f262178b3f5477e50da4091541c2d02a03a3e306ed932f902f5de308cccda1`:

| Transfer/mode | Total CPU | System CPU | Mach syscalls/s |
| --- | ---: | ---: | ---: |
| 128 KiB, bridge | 17.873% | 3.916% | 3578.4 |
| 128 KiB, direct | 17.828% | 3.664% | 2960.1 |
| 256 KiB, direct | 16.684% | 2.487% | 1520.7 |
| 256 KiB, bridge | 17.100% | 2.671% | 1821.6 |

These shorter runs also had the requested USB lengths and no observed
software errors. Their delivered rates were 9.991–9.998 million IQ pairs/s;
stop/join remained below 1.01 ms. The single direct-256 run reached raw queue
high water 8/8 with a 67 ms read gap, versus 2/8 and 17.1 ms in bridge-256;
the sequential single pass does not establish a causal latency difference.
Direct mode's total-CPU improvement was only 0.045 percentage point at 128 KiB
and 0.416 point at 256 KiB in this pass, so it remains diagnostic-only.

Root chose **256 KiB with the bridge** as the integration candidate because
its system CPU and Mach-call reduction were consistent, throughput remained
full-rate and observed stop latency stayed below 3 ms. User-time variation
prevents claiming that 256 KiB decisively beats 128 KiB in total CPU. Raw
inflight/delivery/bridge payload budgets remain exactly 1/2/1 MiB, and no other
platform/receiver defaults change. These are source/synthesis/copy process
measurements, not whole-app decoder CPU or energy acceptance.

The main repository's ignored `macos/build/performance/transport-physical-v2/`
holds original per-run receipts. Root's `root-selection-summary.json` has
SHA-256 `3c6a24c1859db39f2a6865a09175d011c900822cce9f24e8bc09d4988482f13b`.
The canonical graph worker receipt preserves their source/artifact hashes,
settings, counters and limitations for the separate integration review.

## Deterministic evidence and acceptance

Tests compare current synthesis bits, clip totals and continued state across
64/128/256 KiB and odd/short partitions for both filter modes; production
synthesis source is unchanged here. Adapter tests drain large CF32 blocks
using the exact app extent in both receive modes and check bytes/metadata
across retained suffixes, timeout, short read, error and cancellation. Every
candidate's enabled bridge is saturated before stop. Both modes exercise a
real timed-out receive followed by a large block, then stop an in-flight
blocking read after verifying that it owns the read permit. Transport tests fill the 2 MiB raw queue, force known
full/short newest-block drops, check exact raw accounting, and stop while the
producer is parked. Existing stall recovery and cancellation tests remain.

Required local gates are fmt, targeted clippy, debug and Release C ABI/transport
tests, Airspy/cross-crate tests, Release Mac C ABI build, feature-enabled probe
build/tests, diff check and explicitly scoped companion secret scan. Root's
physical A/B source/artifact/CPU/syscall/latency/counter receipt and separate
source review must precede a production choice or graph completion.

No deployment, signing, publication, source push, or receiver action is
performed by the implementation worker. Atomic Mac/header/Kotlin/both-JNI
adoption from one reviewed revision belongs to the separate integrator.
