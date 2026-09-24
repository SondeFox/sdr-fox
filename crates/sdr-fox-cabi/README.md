# C ABI low-rate RTL transfer policy

The macOS C adapter selects 16 KiB native USB transfers only for an RTL
receiver whose last successful sample-rate operation returned 225,001 through
300,000 complex samples/s. Higher, invalid and unknown rates retain the prior
64 KiB extent. Airspy and non-macOS policies are unchanged. The inflight and
delivery counts remain 16 and 32, so the low-rate RTL payload budgets become
256 KiB inflight and 512 KiB queued. These are bounded queues, not a guarantee
against every host stall.

At 250 kS/s, a 16 KiB CU8 transfer contains 8,192 complex samples and takes
32.768 ms to fill, versus 131.072 ms for 64 KiB. This improves delivery latency;
it does not change sample rate, tuning, analog bandwidth or IQ values. A host
that emits one FFT per delivery still needs its own time-based presentation
policy. Smaller caller read buffers alone cannot shorten native transfer fill
time: the C adapter first receives a complete native block and then retains any
unread suffix.

The opaque handle stores the applied rate with the receiver inside its existing
control mutex. Both sample-rate entry points use one transaction: invalidate
old knowledge, call the hardware setter, then publish its successful actual
result. A hardware error leaves the rate unknown; a null output argument does
not attempt hardware or change known state. Stream policy selection and native
startup use the same mutex, so a concurrent start cannot observe half of the
rate transaction. No public C or Rust device API is added, and the generated
C header is unchanged.

The policy, control tests and in-memory CU8 vectors are first-party work under
this repository's MIT OR Apache-2.0 terms, authored against canonical source
`d0eecab44be0e895977fac2d38759a82caf0e116`. The 16 KiB extent matches the
already inventoried SondeFox Android adapter at canonical SondeFox source
`88fd480ea733b385932922608dc0aaf92a01180a`. No external implementation, dependency,
capture or binary fixture is introduced. Tests cover rate/platform boundaries,
actual-rate quantization, both setter entry points, failures and retry state,
concurrent control/start, and exact CU8 continuity/drop metadata across both
transfer sizes and partial caller reads.

Synthetic checks do not establish physical USB cadence, RF rejection, on-air
decoding or final application behavior. The consumer integration must build
the Mac archive/header, Kotlin binding and both JNI libraries from one reviewed
source and qualify that new artifact separately. Earlier hardware passes are
historical evidence for their original source.
