# Android USB board identity correction

The new `SdrFox.openUsbDevice` method and additive
`nativeOpenByFdWithIdentity` JNI entry point retain the original fd-open API.
They pass actual USB VID/PID and exact manufacturer/product strings supplied
by the caller from the same permission-authorized `UsbDevice`. ID conversion
rejects values outside uint16; absent strings remain absent, and no serial is
collected. Device/connection ownership, cancellation, streaming, and close order
are unchanged. Both JNI libraries and the Kotlin binding must be adopted
together; the JNI export set grows from 17 to 18. The Mac C ABI is unchanged.

The old Android entry point fabricated PID 0x2832 and no manufacturer, making
strict V4 identity impossible even if product was `Blog V4`. The existing
board predicate requires 0x0bda/0x2838 plus `RTLSDRBlog`/`Blog V4` and an R828D
tuner. It is unchanged. V4 therefore receives its already implemented 28.8 MHz
tuner clock through the new path; generic R828D retains 16 MHz.

A reported `PLL not locked at 403625000 Hz` while requesting 401.500 MHz is
consistent with the narrowband IF: the tuner synthesizes RF + 2.125 MHz and
reports the LO, not the requested RF. A scripted failed-lock test pins this
relation at 250 kS/s. The failure is not evidence that the user entered the
wrong frequency. Source diagnosis does not identify the reporter's APK version
or substitute for a physical Android test.

All changes are independently authored against canonical clean-history source
`d5ea7d827835455e9fdd10d0a1d6a83f72e2ae60`, under the existing MIT OR Apache-2.0
terms. No new dependency, hardware protocol, external implementation, fixture,
GPL/legacy/research source, or captured radio data was used. The clock facts
and manufacturer references were already recorded in `MACOS_USB.md`; no new
radio register behavior is introduced.

Validation includes JNI descriptor preservation/rejection tests, strict V4
clock-selection tests, the narrowband RF/LO regression, both NDK target builds,
and unchanged C ABI/header verification. Exact counts, source revision,
binaries, target exclusion checks and physical results belong to the consumer
atomic adoption receipt. Unit tests do not certify RF sensitivity, switched
HF/VHF/UHF inputs, notch behavior, bias voltage, on-air decoding or a signed app.
