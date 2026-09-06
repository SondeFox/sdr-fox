# Repository context

## Identity

- **Canonical repository:** `https://github.com/SondeFox/sdr-fox`
- **Canonical branch:** `master`
- **Current visibility:** private
- **Primary consumer:** `https://github.com/SondeFox/SondeFox`

sdr-fox is the SondeFox organization's shared receiver and signal-processing
layer. It supports desktop experimentation, a CLI and language bindings, but
the near-term integration target is the SondeFox Android application through
the Kotlin/JNI surface in `bindings/android` and `sdr-fox-jni`.

## Clean origin

The organization repository is intentionally created from one reviewed source
snapshot with a new root commit. Development history from the former personal
repository is excluded because it contains material that is not approved for
the future public project. The old repository is a restricted archive, not a
Git upstream.

Never add the old repository as a remote, copy its `.git` directory, restore
its tags or branches, or use a platform repository transfer. Needed changes
must be implemented anew against this repository and reviewed under the rules
in [`UPSTREAMS.md`](UPSTREAMS.md).

The import snapshot includes the approved CI repair: Rust formatting, a
bounded conversion at the libusb boundary, and an isolated Python build
environment. It intentionally excludes agent graph state, review transcripts,
generated radio captures/images, and private analysis folders.

## Invariants

- Android receives a framework-authorized USB file descriptor and uses
  `nusb`; Android must not link libusb.
- Desktop contains `nusb` plus a `rusb`/libusb fallback. macOS prefers the
  latter because of observed IOKit control-transfer stalls.
- A stream has bounded delivery, explicit drop accounting, and monotonic
  sequence metadata. Changes must preserve backpressure behavior.
- Unsafe code belongs only at unavoidable FFI/USB boundaries with written
  ownership and lifetime contracts.
- C, Python, JNI, Kotlin, and CLI interfaces describe the same device and
  stream semantics.
- CI actions are pinned to immutable full commit SHAs and the workflow token is
  read-only.

## Public-release gate

Private development does not imply approval to publish. Before changing
visibility, the owner must complete provenance and legal review, dependency
and notice review, secret scanning, hardware validation, documentation review,
and a release-candidate build from a fresh clone. The clean repository must
still have one lineage with no legacy refs or releases.

See [`PROVENANCE-AUDIT.md`](../PROVENANCE-AUDIT.md), [`SECURITY.md`](../SECURITY.md),
and [`UPSTREAMS.md`](UPSTREAMS.md). Future coding agents should update this file
whenever repository identity, integration boundaries, or release gates change.

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
