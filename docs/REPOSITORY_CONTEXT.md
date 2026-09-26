# Repository context

## Identity

- **Canonical repository:** `https://github.com/SondeFox/sdr-fox`
- **Canonical branch:** `master`
- **Current visibility:** public source; external code intake remains closed
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
- macOS uses unmodified `nusb` only; Linux/Windows retain their `rusb`/libusb
  fallback. Historical IOKit OUT stalls were not reproduced in current tests.
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

## Public-source status and release gates

The owner made this clean-history source repository public on 2026-09-26 under
its existing MIT OR Apache-2.0 terms. The decision followed an engineering
provenance review, owner authorship and bot/AI adoption statements, source and
GitHub-surface scans, dependency/notice review, a fresh-clone CLI build, and a
bounded physical Airspy enumeration/open/stream check. That five-second stream
at a negotiated 2.5 MS/s reported zero drops; it did not test RF sensitivity,
on-air decoding, other receiver models, or final application binaries. These
observations are not independent legal or patent certification.

No supported binary release is announced. Before publishing one, qualify its
exact source, build inputs, notices, signer, platform and physical behavior.
External code intake stays closed until versioned agreements, an identity and
authority register, DCO/coauthor coverage, and a protected current-head
clearance check are operating. The clean repository retains one lineage with
no legacy refs or releases.

See [`PROVENANCE-AUDIT.md`](../PROVENANCE-AUDIT.md), [`SECURITY.md`](../SECURITY.md),
and [`UPSTREAMS.md`](UPSTREAMS.md). Future coding agents should update this file
whenever repository identity, integration boundaries, or release gates change.

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

## Current macOS Airspy performance integration

The reviewed c89f580c369b86a874e24669f86779eccb76e9c3 baseline is extended by
independently reviewed packed routing/statistics source
7e50fdd33fa7cbf23add05b96835e5281746aabd and resilience source
c1d3a88ea844572fb59e302193b12289e8fc1690. The separate integration combines
only these frozen source outputs after all four consumer workers completed.

macOS Airspy uses 256 KiB raw transfers, 16 inflight transfers, 16 raw delivery
blocks and two CF32 bridge blocks (4/4/1 MiB nominal payload, +5 MiB). The bridge
remains enabled. The historical 1/2/1 and diagnostic 8/4/1 profiles remain
explicit controls, never environment overrides of production. The chosen
profile passed bounded worker-source trials; it cannot guarantee absorption
of every historical 99–181 ms gap or establish hardware losslessness.

Packed routing/statistics preserve sequential v0→v1 DC/FMA, all output formats,
clipping and per-call carry/reset behavior. Cargo.lock, unmodified nusb 0.2.7,
other receiver/platform policies, rates, calibration and QoS are unchanged.
See MACOS_AIRSPY_PACKED_STATS_CPU.md and MACOS_AIRSPY_RESILIENCE.md.

The consumer's sole graph coordinates isolated worktrees through declared
logical resources; no graph/tool state enters this repository. Actual combined
source, tests, artifact hashes and same-revision Mac/header/Kotlin/both-JNI
adoption belong to the consumer receipt. Source and component success are not
publication, clean-host reproduction, final signed-app or approximately 30% CPU
acceptance. Prior successful and failed physical measurements remain historical.

## Android V4 identity correction (2026-09-09)

Android now offers additive `openUsbDevice`/`nativeOpenByFdWithIdentity` APIs
that preserve actual authorized USB IDs and manufacturer/product metadata.
The existing strict V4 clock predicate is unchanged; generic R828D retains
16 MHz. Existing fd-open callers keep their ABI and behavior. No dependency,
C ABI or radio-register change is included. See
[the implementation and provenance record](ANDROID_USB_IDENTITY.md).

## Android page-size integration (2026-09-24)

The maintained branch integrates the canonical shipped fb34d8c runtime with
master's separate historical reproduction tooling. Android JNI now has
package-local 16 KB LOAD/RELRO linker policy, with no dependency or JNI API
change. See [the page-size contract](ANDROID_PAGE_SIZE.md) for target selection,
tests and the remaining atomic consumer/fresh-build/device gates. Historical
fixed-pin reproduction still builds fb34d8c and is not new-candidate evidence.
