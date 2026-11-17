# Agent instructions

This repository is the clean-history public source home of sdr-fox.

## Before changing code

1. Read `README.md`, `docs/REPOSITORY_CONTEXT.md`, `docs/UPSTREAMS.md`,
   `docs/AI.md`, and the relevant architecture or integration document.
2. Treat this repository's `master` branch as the only source-history base.
   Never fetch, merge, rebase, cherry-pick, or graft commits, tags, PR refs, or
   patches from the restricted legacy repository.
3. Do not add radio captures, location data, device identifiers, credentials,
   generated WAV/PNG files, audit corpora, review transcripts, or private
   analysis directories.

## Working rules

- Use parallel background agents for major work, followed by a separate
  integration pass. Do not use `codex/` in branch names.
- Use `docs/ARCHITECTURE.md` to locate the owning crate and
  `docs/INTEGRATION.md` to trace consumer-facing contracts before editing an
  API. Treat generated explanations as hypotheses until code and tests verify
  them.
- Keep unsafe code confined to the reviewed FFI/USB boundary and document each
  safety contract.
- Keep Android free of `rusb`, `libusb1-sys`, and linked libusb code. Android
  must enter `nusb` with the framework-provided file descriptor.
- Keep the Android `UsbDeviceConnection` alive until after the native device is
  closed; examples must not teach early close.
- Regenerate `bindings/sdr_fox.h` with cbindgen when the C ABI changes and
  update every affected language example together.
- Keep GitHub Actions at immutable full-length commit SHAs with top-level
  `contents: read` unless a narrower job explicitly needs more.
- Update `NOTICE`, `PROVENANCE-AUDIT.md`, and `docs/UPSTREAMS.md` when upstream
  relationships or distributed dependencies change.

## Required validation

Run the closest relevant checks and, for cross-cutting changes, the full set:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --lib --bins --all-targets -- -D warnings
cargo clippy -p sdr-fox-python --lib -- -D warnings
cargo test --workspace --exclude sdr-fox-python --exclude sdr-fox-jni
cargo test -p sdr-fox-tests
```

For Python changes, build in a virtual environment with Maturin and run the
non-hardware pytest suite. For Android changes, run both NDK targets and verify
the resulting shared libraries contain no linked libusb implementation.

Hardware tests are opt-in. State exactly which receiver, host, sample rate, and
test duration were used; never commit the capture.
