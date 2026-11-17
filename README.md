# sdr-fox

One radio API for RTL-SDR and Airspy receivers, written in Rust with C,
Python, Android JNI, and command-line interfaces.

> **Public source:** the APIs may still change. The source is available under
> MIT OR Apache-2.0; no supported binary release or general hardware
> qualification is announced. Never graft the restricted legacy Git history
> into this clean repository.

## What is here

- RTL2832U support for R820T/R820T2/R828D and the legacy E4000; the
  FC0012/FC0013/FC2580 tuners are recognised but not yet driven.
- Airspy R2 and Mini support, including real-to-IQ synthesis for the Mini.
- A bounded multi-transfer streaming engine with loss and sequence metadata.
- Runtime-dispatched IQ conversion: AVX2, baseline SSE2, NEON, and scalar
  fallbacks.
- Bias-T, SpyVerter, AGC, direct sampling, RTL-SDR Blog V4 behavior, and
  reference-clock spur cancellation.
- Rust crates plus a stable-shape C ABI, PyO3 module, Android JNI/Kotlin API,
  and the `sdrfox` CLI.
- Mock transports and tuner buses so most validation runs without hardware.

## Transport behavior

macOS uses the published nusb 0.2.7 backend and excludes rusb/libusb from the
resolved graph.
Linux and Windows prefer nusb and retain their rusb fallback. Physical macOS
validation remains required; see `docs/MACOS_USB.md`.

Android is different: the app obtains a USB file descriptor after the user
grants permission, and the JNI layer enters `nusb` through that descriptor.
The Android target does not compile or link `rusb`/libusb. Keep the Android
`UsbDeviceConnection` open until after the native device is closed. See
[`bindings/android/README.md`](bindings/android/README.md).

## Workspace map

| Path | Purpose |
| --- | --- |
| `sdr-fox-core` | Device, stream, gain, sample, and transport contracts |
| `sdr-fox-transport` | `nusb`, desktop `rusb`, Android-fd, and mock transports |
| `sdr-fox-rtlsdr` | RTL2832U control plane and tuner implementations |
| `sdr-fox-airspy` | Airspy R2/Mini device and IQ synthesis |
| `sdr-fox-simd` | Runtime-dispatched sample conversion and spectra |
| `sdr-fox-dsp` | Demodulation, decoding, filters, and spur cancellation |
| `sdr-fox-cabi` | C ABI and generated header |
| `sdr-fox-python` | PyO3/Maturin bindings |
| `sdr-fox-jni` | Android JNI bridge |
| `sdr-fox-cli` | `sdrfox` command-line application |
| `sdr-fox-tests` | Cross-crate mock integration tests |

The trait-level design is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md),
and consumer examples and contracts are in
[`docs/INTEGRATION.md`](docs/INTEGRATION.md).

## Build and test

CI uses Rust 1.95.0 as its tested compiler baseline, also selected by the manual
[current-pin reproduction workflow](docs/CLEAN_REPRODUCTION.md). `Cargo.toml`
still declares Rust 1.86 as the minimum; compatibility with 1.86 is not
validated by these CI gates. Compiler baseline updates are reviewed changes.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --lib --bins --all-targets -- -D warnings
cargo test --workspace --exclude sdr-fox-python --exclude sdr-fox-jni
cargo test -p sdr-fox-tests
```

Build the Python package in an isolated environment:

```sh
python3 -m venv .venv
source .venv/bin/activate
python -m pip install maturin pytest
cd crates/sdr-fox-python
maturin develop --release
pytest tests/test_bindings.py -k "not hardware"
```

Hardware commands are intentionally opt-in:

```sh
cargo run -p sdr-fox-cli -- devices
cargo run -p sdr-fox-cli -- info -d 0
```

## Provenance and release gate

This repository begins from an approved post-remediation source snapshot, not
from the legacy development history. A prior engineering audit identified a
small set of expression-level concerns; the code was then independently
rewritten, renamed, or removed and the notices were corrected. The concise
record is in [`PROVENANCE-AUDIT.md`](PROVENANCE-AUDIT.md), with upstream roles
in [`docs/UPSTREAMS.md`](docs/UPSTREAMS.md).

The audit is engineering evidence, not legal advice. The
[public-source status](docs/REPOSITORY_CONTEXT.md#public-source-status-and-release-gates)
records the bounded review that preceded visibility and the separate binary
release gates. Legacy commits, tags, pull-request refs, and audit evidence stay
in the restricted archive.

## Security and contributing

Please read [`SECURITY.md`](SECURITY.md) before reporting a vulnerability and
[`CONTRIBUTING.md`](CONTRIBUTING.md) before proposing a change. LLMs have been
used extensively in this project; [`docs/AI.md`](docs/AI.md) explains that use
and how to contribute with AI assistance. Do not include captures containing
location data, device identifiers, credentials, or other private field data in
issues, fixtures, or commits.

## License

The source snapshot is provided under **MIT OR Apache-2.0**, at your option;
see [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE). Third-party acknowledgements and desktop
libusb distribution notes are in [`NOTICE`](NOTICE).

The license files state the current source terms. They do not qualify a
GitHub artifact or signed application release, or open external code intake.
See the [contribution status](CONTRIBUTING.md) before submitting code.

## macOS direct USB update (2026-09-06)

macOS now resolves the published, unmodified nusb 0.2.7 only; rusb and
libusb1-sys are target-excluded just as on Android. Linux/Windows retain their
existing fallback. Controlled physical comparison found no historical
control-OUT stall on this host with the attached RTL-SDR and Airspy receivers;
a speculative synchronous IOKit workaround was therefore removed before
adoption. No nusb source is vendored or patched in the final tree.

Physical testing did reveal a distinct Blog V4 PLL failure: the generic
R828D default clock is 16 MHz, while the manufacturer documents 28.8 MHz for
its V4 board. Strict VID/PID plus published manufacturer/product identity now
selects that board's clock; generic R828D keeps 16 MHz. The V4 then locked and
streamed. This is a clock/transport result, not calibrated RF sensitivity or
whole-radio feature acceptance. Sources and test scope are in
`docs/MACOS_USB.md`.

The C ABI adds stable receiver enumeration/open, applied rate, queried sample
rates/gains, stage controls, IF bandwidth and reference oscillator access.
Existing integer selectors and original-format stream reads remain compatible.

## Current macOS Airspy background processing

macOS Airspy uses 256 KiB raw transfers with 16 inflight transfers, 16 queued
raw blocks and two queued CF32 bridge blocks: fixed 4/4/1 MiB payload budgets.
The synthesis bridge stays enabled. The additional 5 MiB nominal payload budget
was selected from bounded physical comparisons; it is not a guarantee against
all host stalls or hardware loss. Other receiver/platform defaults are unchanged.

Safe packed ADC routing fuses exact raw clipping statistics while preserving
sequential DC arithmetic, filter/output values and streaming carry/reset state.
No dependency, nusb version, radio rate, public ABI or QoS change is included.
The exact source/test limits and rejected alternatives are in
[packed statistics](docs/MACOS_AIRSPY_PACKED_STATS_CPU.md) and
[transport resilience](docs/MACOS_AIRSPY_RESILIENCE.md).

The earlier [transport record](docs/MACOS_AIRSPY_TRANSFER_CPU.md) preserves its
historical 1/2/1 MiB measurements. The [kernel record](docs/MACOS_AIRSPY_KERNEL_CPU.md)
preserves the earlier FIR work. Consumer integration must build Mac/header,
Kotlin binding and both Android JNI artifacts from one reviewed combined source;
component results do not establish a whole-app CPU or energy result.

## Android V4 identity correction (2026-09-09)

Android now offers additive `openUsbDevice`/`nativeOpenByFdWithIdentity` APIs
that preserve actual authorized USB IDs and manufacturer/product metadata.
The existing strict V4 clock predicate is unchanged; generic R828D retains
16 MHz. Existing fd-open callers keep their ABI and behavior. No dependency,
C ABI or radio-register change is included. See
[the implementation and provenance record](docs/ANDROID_USB_IDENTITY.md).
## Blog V4 RF routing update (2026-09-09)

Strictly identified V4 receivers now select the internal HF/VHF/UHF path,
apply built-in HF conversion, and program the measured notch/GPIO fields.
The [RF contract](docs/BLOG_V4_RF_ROUTING.md) records independent measurement
provenance, the exact-boundary decision and remaining RF qualification gates.
