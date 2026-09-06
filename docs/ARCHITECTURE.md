# sdr-fox architecture

This document is the map for an LLM or human integrating with or extending
sdr-fox. It covers the crate graph, trait design, and the location of each
current design constraint.

## Crate graph

```
sdr-fox-core        traits, errors, sample/gain/device types (no I/O)
   ▲
   │ depends on
   │
sdr-fox-simd        SIMD/scalar cu8→cf32 + FFT spectrum  (depends on core)
sdr-fox-dsp         demodulators (WBFM/NBFM/AM/SSB) + ADS-B + WAV/PNG writers
                     (depends on core)
sdr-fox-transport   Transport trait + nusb + desktop rusb + mock + streaming
   ▲                 (depends on core, simd)
   │
sdr-fox-rtlsdr      RTL2832 control plane + R82xx/E4000 tuners  (core, transport, simd)
sdr-fox-airspy      Airspy R2/Mini + the 2× decimation fix (core, transport, simd)
   │
   ├── sdr-fox-cabi     stable extern "C" surface  (core, rtlsdr, airspy, transport)
   ├── sdr-fox-cli      `sdrfox` binary             (everything)
   ├── sdr-fox-python   PyO3 bindings               (core, rtlsdr, airspy, transport, simd)
   ├── sdr-fox-jni      Android .so + Kotlin API    (core, rtlsdr, airspy, transport)
   ├── sdr-fox-hardware live-hardware validation tests, all `#[ignore]`
   │                     (core, transport, rtlsdr, airspy, simd, dsp)
   └── sdr-fox-tests    cross-crate integration     (everything)
```

## The three core traits

Everything polymorphic goes through three traits in `sdr-fox-core`:

### `SdrDevice` (the device surface)
The unified API for RTL-SDR and Airspy (modeled on SondeFox's Kotlin
`SdrSource`). `set_frequency` is the single point where the SpyVerter
upconverter offset is applied. `start_stream` returns a `Box<dyn StreamSink>`
(the boxed alias) so core doesn't pull channel/thread deps.

### `Transport` (the USB I/O)
`control_in/out`, `bulk_read`, `start_bulk_stream`. Four impls:
- `NusbTransport` — pure-Rust Linux/Windows desktop path (nusb, no libusb).
- `RusbTransport` — device-local libusb context, including the macOS default.
- `MockTransport` — records requests, replays scripted replies. **The test
  backbone** for every driver crate.
- `NusbFdTransport` — Android fd-injection path. Enters nusb through
  `Device::from_fd` with the framework-supplied descriptor, so the Android
  build contains no libusb code. The only transport compiled on Android.

### `Tuner` + `TunerBus` (the chip layer)
Tuners talk to a `TunerBus` (the RTL2832 I2C repeater in production, a mock in
tests) so they're unit-testable without a transport. One module per chip.

## Design traceability

| Constraint | Where it lives |
|---|---|
| Multi-transfer ring + bounded drop-newest delivery | `sdr-fox-transport/src/stream.rs` |
| Typed errors + `is_disconnected` | `sdr-fox-core/src/error.rs` |
| Runtime conversion dispatch (AVX2/SSE2 on x86_64; measured scalar on aarch64) | `sdr-fox-simd/src/convert.rs` |
| Mockable transport | `sdr-fox-transport/src/mock.rs` |
| RTL2832 block/address encoding and page bits | `sdr-fox-rtlsdr/src/rtl2832/mod.rs` |
| R820T2 PLL and gain behavior | `sdr-fox-rtlsdr/src/tuners/r82xx.rs` |
| SpyVerter single-point offset | `sdr-fox-core/src/device.rs` (`Upconverter`) |
| Airspy Mini 2× real-sample decimation | `sdr-fox-airspy/src/iq_synth.rs` |
| Caller-owned direct buffers with no native JVM upcalls | `sdr-fox-jni/src/jni_impl.rs` |
| Panic containment at the C boundary | `sdr-fox-cabi/src/lib.rs` (`catch_unwind`) |
| nusb on Linux/Windows/Android; rusb fallback and macOS default | `sdr-fox-transport/src/lib.rs` |

## Performance characteristics (measured)

- **cu8→cf32 conversion**: ~10 GB/s input throughput for the 262 KB blocks the
  transport delivers (Apple M-series). An earlier "5.1× SIMD speedup" claim was
  refuted by re-measurement: on aarch64 the margin over scalar is ~1.05× at
  best, and the hand-written NEON kernel measured 16% *slower* than the
  autovectorized scalar loop, so `select_backend` deliberately dispatches the
  scalar path on aarch64 (see the rationale comment in
  `sdr-fox-simd/src/convert.rs`). x86_64 still selects
  AVX2/SSE2 at runtime. The kernel expands one input byte to four output
  bytes, so it is store-bandwidth bound and intrinsics have no headroom.
  Do not cite a SIMD multiplier for aarch64 (Android is aarch64).
- **Streaming ring**: 16 in-flight transfers × 64 KB with non-blocking,
  explicitly counted drop-newest overload handling.
- **Release profile**: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`.

## Known limitations (documented, not silent)

The R82xx family (R820T2 is the deep implementation) and the E4000 are driven.
The E4000 is zero-IF: the demod runs with IF = 0, no spectrum inversion, and
both ADC inputs, unlike the R82xx low-IF path. FC0012/FC0013/FC2580 tuners and
Blog V4 detection are trait-wired but not implemented; a probe that finds one
of those chips — or no tuner at all — fails the open with
`TunerError::NoSupportedTuner` rather than a misleading PLL error. macOS
defaults to a device-local rusb context because nusb control-OUT can stall on
live RTL-SDR hardware. USB teardown, Android fd streaming, and throughput have
mock coverage but still require physical-device release gates.

## macOS direct USB update (2026-09-06)

macOS now resolves nusb only; rusb and libusb1-sys are target-excluded just as
on Android. Linux/Windows retain their existing fallback. The pinned nusb
0.2.7 source under `vendor/nusb` changes macOS control OUT to synchronous
IOKit `DeviceRequestTO`, retaining request/payload ownership and checking
completion length, while leaving bulk and IN event loops intact. This is a
candidate repair for the previously documented asynchronous OUT stall, with
mock request tests; **no attached RTL-SDR or Airspy was available to reproduce
the original failure or validate the repair on hardware**. See
`docs/MACOS_USB.md` for exact validation and provenance.

The C ABI adds stable receiver enumeration/open, applied rate, queried sample
rates/gains, stage controls, IF bandwidth and reference oscillator access.
Existing integer selectors and original-format stream reads remain compatible.
