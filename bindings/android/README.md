# sdr-fox Android integration

This directory holds the Kotlin API (`SdrFox.kt`), the USB-permission helper
(`SdrUsbManager.kt`), and the reference Gradle setup for packaging the native
`.so` into an APK.

## Build the native library

The `.so` is built with `cargo-ndk` for `arm64-v8a` and `x86_64`:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$HOME/Library/Android/sdk/ndk/27.2.12479018  # or your NDK
export SDR_FOX_SOURCE_ROOT="$(pwd -P)"
export CARGO_CACHE_ROOT="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="--remap-path-prefix=${SDR_FOX_SOURCE_ROOT}=/workspace/sdr-fox \
--remap-path-prefix=${CARGO_CACHE_ROOT}=/cargo \
--remap-path-prefix=${HOME}=/home/builder"
cargo ndk -t arm64-v8a -t x86_64 build --release -p sdr-fox-jni --features android
```

This produces `target/<triple>/release/libsdr_fox_jni.so` for each ABI.
The remapping flags keep developer-specific source, Cargo-cache, and home paths
out of panic metadata in the distributed libraries. Treat those flags as part
of the reproducible Android build contract, not as optional local cleanup.

### How Android opens a USB device

Android gives an unprivileged process no way to walk the USB bus, so a device
arrives as a file descriptor the JVM hands down after the user grants
permission via `UsbManager`. `enumerate_usb_devices()` is compiled out on
Android for that reason, and `SdrUsbManager.kt` performs discovery on the Java
side.

The native transport enters nusb (pure-Rust, Apache-2.0 OR MIT) through
`nusb::Device::from_fd` with that descriptor — the same usbfs backend the
desktop uses, just reached via its fd entry point. The streaming ring
(`NusbBufferSource`) is shared with the desktop nusb backend, so `StreamStats`
and the bounded-queue drop policy mean the same thing on both. See
`crates/sdr-fox-transport/src/nusb_fd.rs` and `docs/LIBUSB-REMOVAL-PLAN.md`.

### No libusb code in the Android target

`rusb` and `libusb1-sys` are excluded from the Android target by Cargo target
tables in `crates/sdr-fox-transport/Cargo.toml`, so the Android `.so` links
**no libusb code**. libusb remains the desktop macOS default; desktop binary
distribution notes are documented in §4 of the repository `NOTICE`.

Verify this before shipping an APK. `[profile.release]` sets `strip = "symbols"`
with fat LTO, so the absence of libusb is *not* visible in the dynamic symbol
table — check the embedded strings instead:

```sh
NDK=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/*/bin
SO=target/aarch64-linux-android/release/libsdr_fox_jni.so

strings -a $SO | grep -c -i libusb        # must be 0
strings -a $SO | grep -F -c "$HOME"        # must be 0
$NDK/llvm-readelf -d $SO | grep NEEDED    # liblog/libdl/libm/libc
$NDK/llvm-nm -D --defined-only $SO | grep -c Java_com_sdrfox_SdrFox_native  # 17
```

On the current build those report 0, the four Android system libraries above,
and 17 respectively.

## Gradle integration

In `android/app/build.gradle.kts`:

```kotlin
android {
    defaultConfig {
        minSdk = 29
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
        externalNativeBuild {
            cmake { path = file("src/main/cpp/CMakeLists.txt") }
        }
    }
    // Point externalNativeBuild at a CMakeLists that just copies the prebuilt
    // .so into the APK, OR use a Gradle task to copy them into jniLibs/.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
}
```

The simplest packaging: copy the prebuilt `.so` files into
`android/app/src/main/jniLibs/<abi>/libsdr_fox_jni.so`.

## Usage

```kotlin
val device = usbManager.deviceList.values.first { SdrUsbIds.matches(it) != null }
SdrUsbPermission.request(context, usbManager, device) { granted ->
    if (!granted) return@request
    val (connection, fd) = usbManager.openSdr(device) ?: return@request
    try {
        val kind = SdrUsbIds.matches(device) ?: return@request
        SdrFox.open(fd, kind, device.productName)?.use { sdr ->
            sdr.frequency = 100_000_000L
            sdr.setSampleRate(2_048_000)
            sdr.biasTee = true
            sdr.startStream().use { stream ->
                val samples = java.nio.ByteBuffer.allocateDirect(65_536)
                while (!Thread.currentThread().isInterrupted) {
                    val copied = stream.read(samples, timeoutMs = 250)
                    if (copied == 0) continue // finite timeout; stream remains live
                    samples.flip()
                    // Consume native-endian CU8 bytes here.
                    samples.clear()
                }
            }
        }
    } finally {
        connection.close() // Close second, after SdrFox.close().
    }
}
```

Pass `UsbDevice.productName` so Airspy Mini is not silently configured as an
R2. For a ROM that omits product strings, call `SdrFox.open` with
`Kind.AIRSPY_MINI` explicitly.

### Hold the `UsbDeviceConnection` open

`SdrFox.open` duplicates the framework fd, but **the `UsbDeviceConnection` must
stay open until after `SdrFox.close()`** — close it second, never first.

`dup(2)` does not give the duplicate an independent life: both descriptors share
a single open file description, and usbfs keeps its per-connection state
(claimed interfaces, URB context) on that shared description. Closing the
framework connection early tears that state down under the native side.

The resulting failure is easy to misdiagnose as broken hardware. The descriptor
stays valid and **control transfers keep working**, so open, tuning, sample rate
and gain all succeed — while the **bulk endpoint goes permanently silent**, every
URB pending until cancelled. That is indistinguishable from a wedged RTL2832U,
and no amount of driver-side recovery (demod power-cycle, USB port reset) fixes
it, because nothing is actually wrong with the device.

## JNI thread safety

Streaming is a pull API: Kotlin supplies a writable direct `ByteBuffer`, JNI
copies at most one native block into it, and advances the buffer position on
return. Native worker threads never enter the JVM, so there are no callbacks,
thread attachments, `JNI_OnLoad` VM caches, or per-block Java allocations.
Stream handles use monotonic Arc-backed registries; stop/close are idempotent,
and closing a device closes all Kotlin stream wrappers first.

CU8/CS8 are byte streams. Before creating CS16 or CF32 typed views, set the
buffer to `ByteOrder.nativeOrder()` so Kotlin interprets the native-endian
sample payload correctly.

## Status

- The Kotlin API, USB-permission helper, fd-owning cleanup path, and direct-
  buffer JNI pull bridge are structurally complete and host compile-checked.
- The `cargo ndk` build and on-device validation remain CI/device tasks when an
  Android NDK and USB hardware are available.
