# Bounded macOS Airspy resilience diagnostics

This change adds opt-in fixed payload budgets to the existing physical probe.
Production macOS Airspy remains at 256 KiB raw transfers, four inflight transfers,
eight raw queue blocks and two synthesis bridge blocks. Other receiver/platform
policies and the global `StreamConfig` defaults are unchanged. No environment
variable or public C ABI selector changes production behavior.

| Probe profile | Raw transfer | USB inflight blocks / bytes | Raw queue blocks / bytes | CF32 bridge blocks / bytes | Total payload | Added payload |
|---|---|---|---|---|---|---|
| `baseline` | 256 KiB | 4 / 1 MiB | 8 / 2 MiB | 2 / 1 MiB | 4 MiB | 0 |
| `4-4-1` | 256 KiB | 16 / 4 MiB | 16 / 4 MiB | 2 / 1 MiB | 9 MiB | 5 MiB |
| `8-4-1` | 256 KiB | 32 / 8 MiB | 16 / 4 MiB | 2 / 1 MiB | 13 MiB | 9 MiB |

The selector accepts exactly these profiles. The larger profiles require
`--kib 256 --bridge 1`. No user-supplied arbitrary size/count is converted to an
allocation. The historical baseline 64/128/256 KiB transfer diagnostic remains
available, including its explicit direct-mode comparison. Omitting the new
selector retains that baseline behavior. The unchanged generic transport API
still accepts valid caller-provided configurations; these diagnostic limits are
not a new global transport restriction.

At 10 million complex samples/s, Airspy transports 20 million 16-bit real ADC
containers/s: 40 MB/s. One MiB of inflight raw payload covers 26.2144 ms, four
MiB covers 104.8576 ms, and eight MiB covers 209.7152 ms. The two/four MiB raw
queues cover 52.4288/104.8576 ms respectively. Host-observed reaping/read gaps
of roughly 99–181 ms motivate the experiment. Four MiB cannot cover a 181 ms
reaping pause; eight MiB only adds nominal coverage. Queue and ring coverage
are different stages and must not be added as a guarantee against hardware loss.

These are reserved stage payload budgets, not process RSS limits. They exclude
allocation overhead, the worker's current raw block, synthesis scratch/output,
a blocked bridge sender, retained C read suffix and caller output buffer. A
short transfer can use less payload than its reserved capacity. Larger queues
also permit more latency before the consumer catches up. CPU and RSS must be
measured externally alongside drop/throughput and latency results.

## Root-operated probe

Only the coordinator may stop the app, claim the receiver and run this probe.
The probe requires `--exclusive-hardware` and every RF control explicitly. Use
identical receiver, host, frequency, gain, AGC, bias, 10 MS/s rate and duration
across matched arms. This example shows the existing synthetic-test control
values; the coordinator supplies the authorized physical settings:

```sh
cargo build -p sdr-fox-cabi --release --features transfer-probe --example macos_transfer_probe
macos_transfer_probe --exclusive-hardware --kib 256 --payload-profile 4-4-1 --seconds 90 --frequency 404000000 --rate 10000000 --lna 140 --mixer 150 --vga 150 --lna-agc 0 --mixer-agc 0 --bias 0 --bridge 1
```

Use `baseline`, `4-4-1`, and `8-4-1` for counterbalanced comparisons. The probe
emits only aggregate numbers and explicit RF settings. It discards sample
payloads, never prints receiver descriptors and does not write captures. The
observer's mutex/histogram work is present in every arm and contributes to
probe overhead. Profile parsing and all tests are hardware-free.

`payload_budget` reports selected profile, exact active stage capacities and
nominal raw-rate coverage. `complete` retains the existing fields, including
app first/interarrival distributions, stop-and-join latency, queue high water,
sequence/drop estimates, read errors/timeouts and successful USB size histogram.
`measurement_windows` adds these distinct accounting boundaries:

- C ABI copied bytes and IQ/s use the timed app receive interval, which starts
  before stream creation. USB startup and first-read latency are included.
  Throughput fields are integer floors; exact counters and nanoseconds remain
  available for fractional analysis.
- The pre-stop USB count and arrivals are a lock-consistent snapshot taken
  just after timed app reads end, divided by that snapshot's own elapsed time.
- The legacy successful USB total includes observed successful reaps after
  that snapshot while closing/joining. It is not divided by the app interval.
  `successful_usb_bytes_after_snapshot` identifies that difference. It does
  not claim to count every native completion discarded inside backend Drop.
- Raw bytes delivered/drop blocks before stop are a separate transport stats
  snapshot. Final raw high-water count times configured raw transfer size is
  an upper bound on queued payload, not an exact occupancy/RSS measurement.

USB arrival times observe host reaping, not device timestamps. Raw-pair loss
estimates use transport two-byte pairs and differ from Airspy complex-IQ loss.
The receiver exposes no hardware loss counter through this path; zero observed
software drops does not establish zero device-side loss. Final signed-app
acceptance must also inspect source, native and joint-lane delivery separately.

The unchanged backend destructor cancels and reaps pending transfers with a
50 ms timeout per completion until one wait times out. Larger rings can increase
total stop latency when completions return slowly. Tests prove bounded recovery
poll attempts and preservation of pending ownership, not a new total destructor
deadline or a physical stop bound. Root must measure stop-and-join for every arm.

## Software evidence and provenance

The implementation and deterministic synthetic tests are authored first-party
from canonical `https://github.com/SondeFox/sdr-fox.git` base
`c89f580c369b86a874e24669f86779eccb76e9c3`, tree
`76cdb32545e3d0ddda784c054df8879dfe4f5656`. They are distributed under this
repository's MIT OR Apache-2.0 terms with its existing license texts. No copied
external implementation, new dependency, recorded sample, generated binary or
fixture is added. `Cargo.lock` and published unmodified nusb 0.2.7 are unchanged.
The consumer's canonical graph receipt binds the final source revision and
actual command/log hashes; coordinator graph state is not copied here.

Policy tests prove fixed count/byte products and invalid selection rejection,
including `usize::MAX`, and unchanged non-Mac/non-Airspy defaults. Channel-gated
transport tests fill each queue without consuming, drop two full blocks plus
514 bytes, drain exact surviving payloads, then release a post-drop survivor.
They require three drops, 262401 raw-pair estimate and the exact sequence gap,
with zero transfer/unknown-overrun errors. Stop wakes a parked source while a
queue is full and verifies one destruction after join. Existing native recovery
seams exercise 0/4/16/32 pending completions, timeout, stop after partial reap
and retention of an owned clear-operation lease until its original completion.
C ABI tests cover both new profiles' exact suffix copies, metadata, timeout,
pending read cancellation and saturated synthesis bridge shutdown.

No physical result or production winner is established by this software change.
The coordinator records matched hardware/RSS/CPU results and selects any later
production change only after independent review. Additional budgets beyond
8/4/1 MiB require a separate proposal.
