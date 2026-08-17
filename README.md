# sdr-fox

One radio API for RTL-SDR and Airspy receivers, written in Rust with C,
Python, Android JNI, and command-line interfaces.

> **Private incubation:** the implementation is active and its APIs may still
> change. Public release is gated on provenance, security, packaging, and
> hardware review. Do not publish this repository or graft its legacy Git
> history into this clean snapshot.

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

The desktop build includes both `nusb` and `rusb` transports. Linux and
Windows prefer the pure-Rust `nusb` backend and can fall back to `rusb`;
macOS prefers `rusb`/libusb because the IOKit path has stalled RTL2832U
control-OUT transfers in hardware testing.

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

Rust 1.86 or newer is required.

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

The audit is engineering evidence, not legal advice. Public release remains a
deliberate review gate, and legacy commits, tags, pull-request refs, and audit
evidence must stay in the restricted archive.

## Security and contributing

Please read [`SECURITY.md`](SECURITY.md) before reporting a vulnerability and
[`CONTRIBUTING.md`](CONTRIBUTING.md) before proposing a change. Do not include
captures containing location data, device identifiers, credentials, or other
private field data in issues, fixtures, or commits.

## License

The source snapshot is provided under **MIT OR Apache-2.0**, at your option;
see [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE). Third-party acknowledgements and desktop
libusb distribution notes are in [`NOTICE`](NOTICE).

Repository access is private during incubation. Making the project public is
a separate approval, not an implication of the license files being present.
