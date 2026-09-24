# Integrating with sdr-fox

**Audience: an LLM (or human) writing code that consumes sdr-fox.** This doc
gives the 5-line happy path in each language, the contracts, and the gotchas.

## Rust (the native API)

```rust
use sdr_fox_core::{StreamConfig, IqFormat, Upconverter};
use sdr_fox_transport::NusbTransport;
use sdr_fox_rtlsdr::RtlSdrBackend;
use sdr_fox_core::{DeviceDescriptor, DeviceKind, SdrBackend, Transport};

let transport: Box<dyn Transport> = Box::new(NusbTransport::open(0x0bda, 0x2832, 0)?);
let desc = DeviceDescriptor { vendor_id: 0x0bda, product_id: 0x2832, kind: DeviceKind::RtlSdr, ..Default::default() };
let mut dev = RtlSdrBackend.open(&desc, transport)?;
dev.set_sample_rate(2_400_000)?;
dev.set_frequency(100_000_000)?;          // SpyVerter offset applied here if set
dev.set_upconverter(Some(Upconverter::spyverter()))?;  // 120 MHz HF upconverter
let mut stream = dev.start_stream(StreamConfig { format: IqFormat::Cf32, ..Default::default() })?;
while let Some(Ok(block)) = stream.recv() {
    if let sdr_fox_core::IqSamples::Cf32(f) = &block.samples { /* use f */ }
}
```

## C (the generated header)

```c
#include "bindings/sdr_fox.h"
SdrFoxDevice *dev = NULL;
if (sdrfox_open_index(0, SdrFoxKind_Rtlsdr, &dev)) { /* error string */ }
sdrfox_set_frequency(dev, 100000000);
sdrfox_enable_spyverter(dev);
SdrFoxStream *s = NULL;
sdrfox_start_stream(dev, SdrFoxFormat_Cf32, &s);
uint8_t buf[65536];
intptr_t n = sdrfox_read_stream(s, buf, sizeof(buf), 1000);
if (n < 0) { /* read error */ }
SdrFoxStreamStats stats = {0};
sdrfox_stream_stats(s, &stats);
sdrfox_stop_stream(s);
sdrfox_close_stream(s);
sdrfox_close(dev);
```

## Python (PyO3, after `maturin develop`)

```python
import sdr_fox
with sdr_fox.SdrFox.open(0, kind="rtl-sdr") as sdr:
    sdr.frequency = 100_000_000
    sdr.gain = 400                      # 40.0 dB (tenths)
    sdr.enable_spyverter()
    block = sdr.read_block(format="cf32", timeout_ms=1000)
    if block is not None:
        # samples is a native-endian bytes payload; metadata exposes loss.
        print(block.sequence, block.dropped, len(block.samples))
    stats = sdr.stream_stats()           # one coherent snapshot
```

Offline SIMD conversion (no device needed):
```python
floats = sdr_fox.convert_cu8_to_cf32(raw_cu8_bytes)
```

## Kotlin (Android JNI)

```kotlin
val (connection, fd) = usbManager.openSdr(device) ?: return
try {
    SdrFox.open(fd, SdrFox.Kind.RTL_SDR, device.productName)?.use {
        it.frequency = 100_000_000L
        it.setSampleRate(2_400_000)
        it.biasTee = true
        it.startStream().use { stream ->
            val samples = java.nio.ByteBuffer.allocateDirect(65_536)
            if (stream.read(samples, timeoutMs = 250) == 0) {
                // Finite timeout: the stream remains live and may be read again.
            }
        }
    }
} finally {
    connection.close() // The framework connection must be closed second.
}
```

## CLI

```sh
sdrfox devices
sdrfox info -d 0
sdrfox capture -d 0 -f 100e6 -s 2.4e6 -n 1000000 -o out.cu8
sdrfox stream -d 0 -f 100e6 --waterfall   # ASCII dBFS spectrum to stderr
sdrfox biast -d 0 --on
```

## Contracts

### Sample formats (`IqFormat`)
- `Cu8`: interleaved unsigned 8-bit IQ (RTL-SDR native), 2 bytes/sample.
- `Cs8`, `Cs16`: signed variants.
- `Cf32`: interleaved float IQ in ≈[-1, 1], 8 bytes/sample. Produced by SIMD
  conversion of cu8.

### Streaming
`StreamHandle` is `Box<dyn StreamSink>`. `recv()` returns:
- `Some(Ok(block))` — an `IqBlock { samples, dropped, sequence, timestamp }`.
- `Some(Err(e))` — a fatal worker error (check `e.is_disconnected()`).
- `None` — stream ended (cancel or device gone).

`recv_deadline(Instant)` has the same result shape, except an elapsed deadline
is `Some(Err(SdrError::Timeout))` and is non-terminal: a later receive remains
valid.

Delivery is bounded and non-blocking. Under overload the current newest block
is dropped so USB reaping never stalls; `IqBlock::sequence` exposes the gap and
`dropped` reports cumulative sample loss. This favors continuous hardware
service and ordered delivery over unbounded memory or producer blocking.

### Error model
Two-level `thiserror` enum: `SdrError` (top) wraps `TunerError`. Use
`is_disconnected()` to decide retry-vs-give-up. `SdrError` is `#[non_exhaustive]`
— match `_` for forward-compat.

### Upconverter / SpyVerter
`Upconverter::spyverter()` = 120 MHz LO, non-inverting. **Always speak the true
RF frequency**; the offset is applied inside `set_frequency` at exactly one
point. Set it via `set_upconverter(Some(...))` before tuning.

### Reference-clock spurs ("birdies")
The receiver's own crystal radiates harmonics that its front end then receives,
at fixed RF frequencies. On a 28.8 MHz RTL-SDR they land every 28.8 MHz —
144.000, 403.200, 432.000, 460.800 and up. Measured on an R820T2 with an
antenna attached: **30–50 dB above the noise floor**, which is fatal for
anything narrowband on the same channel (403.200 is a standard radiosonde
channel). They cannot be tuned away: the spur sits at the same RF as the
signal, so moving the LO moves both.

They can be cancelled, because the harmonic and the ADC sample clock come from
the same crystal. The spur therefore sits at an exact, drift-free normalized
frequency — measured at under 1 Hz wide and stable to 0.014 Hz over 40 minutes
of warm-up — so it can be removed by projection rather than notched out:

```rust
use sdr_fox_dsp::SpurCanceller;

let mut spurs = match dev.reference_clock_hz() {          // None => unknown, skip
    Some(reference) => SpurCanceller::for_reference(
        f64::from(reference), frequency as f64, f64::from(actual_rate)),
    None => SpurCanceller::new(f64::from(actual_rate)),
};
let mut acquired = false;
// per block of interleaved cf32:
if !acquired { acquired = spurs.acquire(&iq); }          // false => block too short, retry
spurs.process(&mut iq);                                   // in place
```

`acquire` is what makes it work: the nominal harmonic is tens of Hz off because
of tuner PLL quantization, and skipping it leaves a visible residual. Run it
once — the spur does not drift. Measured suppression on real captures is
**33–50 dB**, putting the spur ~10 dB *below* the surrounding noise floor,
while spectrum more than 500 Hz away changes by 0.000017 dB on average.

**Cancel after the last quantization step, never before.** This is the one way
to get it wrong, and it fails quietly. Measured on a real capture, spur level
relative to the local noise floor:

| | spur vs floor |
|---|---|
| original cu8 from the device | +30.15 dB |
| cancelled, kept in f32 | **−9.27 dB** |
| cancelled, then requantized to cu8 | +6.37 dB |
| cancelled, then requantized to cs16 | **−9.27 dB** |

The spur was −37.7 dBFS — an amplitude of just **1.65 cu8 LSB**, versus 425
cs16 LSB. The correction being subtracted is therefore barely more than one
8-bit quantization step, and rounding back to cu8 discards most of it: 15.6 dB
of the 39 dB benefit is lost. 16-bit and float preserve it exactly.

So a consumer streaming cu8 (the cheapest format over JNI) must cancel on its
own float conversion, not ask the driver to cancel into the cu8 it hands over.
This is why the canceller is a consumer-side DSP stage rather than something
the driver applies to the stream.

Frequencies are **offsets from the tuned centre**, so rebuild the canceller and
re-run `acquire` after any retune. Clip/rail counting should stay on the raw
samples, ahead of cancellation, since cancellation changes sample values.

`sdrfox demod` applies it automatically; `sdrfox capture` deliberately does
not, so raw captures stay raw.

## Gotchas

- **macOS live hardware**: the backend uses the published, unmodified nusb
  0.2.7. The C ABI selects 256 KiB raw transfers for macOS Airspy, with
  fixed 4/4/1 MiB inflight/raw/CF32 payload budgets (16/16/2 blocks) and
  its synthesis bridge retained. The prior 1/2/1 profile remains a diagnostic
  control; the larger budget adds 5 MiB, not a CPU or losslessness guarantee.
  Other receiver/platform
  defaults are unchanged. Small aligned reads preserve the unread suffix of a
  synthesized block; one C read is not necessarily one USB completion.
- **Android**: use `SdrFox.open(fd, kind, productName)` with the fd from
  `UsbDeviceConnection.getFileDescriptor()`. USB permission is the app's job
  (`SdrUsbPermission.request`). Streams pull into writable direct
  `ByteBuffer`s; use `ByteOrder.nativeOrder()` before CS16/CF32 typed views.
- **Airspy Mini**: streams REAL samples at 2× the IQ rate; `IqSynthesizer`
  decimates by 2 automatically. Don't apply your own decimation.
- **`closest_gain(desired_tenths_db)`** snaps arbitrary UI input to the nearest
  hardware step (uses `i64` to avoid `i32::MIN` overflow).
