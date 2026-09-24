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
- Normal CI fixes Rust at 1.95.0, matching the current-pin reproduction
  toolchain. The declared Rust 1.86 minimum remains unverified by these gates;
  compiler baseline updates require review.

## Public-release gate

Private development does not imply approval to publish. Before changing
visibility, the owner must complete provenance and legal review, dependency
and notice review, secret scanning, hardware validation, documentation review,
and a release-candidate build from a fresh clone. The clean repository must
still have one lineage with no legacy refs or releases.

See [`PROVENANCE-AUDIT.md`](../PROVENANCE-AUDIT.md), [`SECURITY.md`](../SECURITY.md),
and [`UPSTREAMS.md`](UPSTREAMS.md). Future coding agents should update this file
whenever repository identity, integration boundaries, or release gates change.
