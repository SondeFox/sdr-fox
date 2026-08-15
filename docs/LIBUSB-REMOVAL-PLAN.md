# Android/desktop USB boundary

**Status:** Android removal complete; desktop libusb retained intentionally  
**Last reviewed:** 2026-08-14

This path is retained because older integration notes link to it. It now
records the implemented boundary rather than an outstanding plan.

## Android

Android receives a `UsbDeviceConnection` file descriptor after the app obtains
USB permission. `sdr-fox-jni` duplicates the descriptor and
`NusbFdTransport` enters nusb through `Device::from_fd`. Cargo target tables
exclude both `rusb` and `libusb1-sys` from Android, and desktop-only modules are
compiled behind `cfg(not(target_os = "android"))`.

The framework `UsbDeviceConnection` must remain open for the native device's
entire lifetime. A duplicated descriptor shares the usbfs connection state;
closing the framework object first can leave control transfers working while
bulk URBs remain silent. Close `SdrFox` first and the framework connection
second.

Before shipping an Android build, verify both supported ABIs and inspect the
result rather than assuming Cargo target selection worked:

```sh
cargo ndk -t arm64-v8a -t x86_64 build --release \
  -p sdr-fox-jni --features android

SO=target/aarch64-linux-android/release/libsdr_fox_jni.so
strings -a "$SO" | grep -c -i libusb
llvm-readelf -d "$SO" | grep NEEDED
llvm-nm -D --defined-only "$SO" | grep -c Java_com_sdrfox_SdrFox_native
```

The libusb string count must be zero. Review the expected system-library and
JNI-symbol counts when the NDK or bindings change.

## Desktop

Desktop builds intentionally retain two backends:

- Linux and Windows prefer nusb and can fall back to rusb/libusb.
- macOS prefers rusb/libusb because RTL2832U control-OUT transfers have stalled
  through the IOKit nusb path in hardware testing.

This means a distributed desktop binary can carry libusb's LGPL-2.1-or-later
requirements. Prefer dynamic linkage and verify the final binary. The Android
result does not imply anything about desktop linkage.

## Maintenance guardrails

- Keep desktop dependencies in a `cfg(not(target_os = "android"))` target
  table; Cargo features are additive and are not a reliable exclusion boundary.
- Keep Android transport tests and both NDK targets in CI.
- Treat file-descriptor ownership, interface claims, cancellation, and close
  order as security- and reliability-sensitive contracts.
- Update `NOTICE`, the Android binding README, and consumer documentation when
  this boundary changes.
