# Upstreams and source-use policy

"Upstream" can mean either a Git remote or a technical reference. Keep those
meanings separate.

## Git repositories

| Role | Location | Policy |
| --- | --- | --- |
| Canonical origin | `https://github.com/SondeFox/sdr-fox.git` | The only approved Git upstream for this project |
| Android consumer | `https://github.com/SondeFox/SondeFox.git` | Pins reviewed sdr-fox source/binaries; never vendors this repository's history |
| Legacy development archive | Restricted personal repository | Archive only; never add as a remote or import commits, tags, PR refs, patches, releases, or `.git` data |

If a local checkout contains any other push URL, stop and correct it before
publishing. Do not rely on redirects from the former personal location.

## Technical references and dependencies

| Project | Role | License posture | Allowed use |
| --- | --- | --- | --- |
| Osmocom rtl-sdr | Public hardware-protocol reference | GPL-2.0-or-later | Interface facts and hardware behavior only; do not copy implementation expression |
| desperado rs-rtl | Streaming-design acknowledgement | MIT | Retain its copyright/license acknowledgement when applicable |
| nusb | Desktop and Android USB dependency | Apache-2.0 OR MIT | Consume through Cargo; preserve resolved license metadata |
| rusb / libusb | Desktop USB fallback | rusb is permissive; libusb is LGPL-2.1-or-later | Desktop only; review dynamic/static linking obligations before distributing binaries |

The resolved dependency graph in `Cargo.lock` is authoritative for versions,
not this overview. `NOTICE` is authoritative for distributed acknowledgements.

## Rule for future updates

Before using a new reference or upstream version:

1. Record its exact URL, revision or release, license, and intended use in the
   change description.
2. Prefer public specifications, datasheets, measurements, and permissively
   licensed dependencies.
3. For copyleft implementations, extract only necessary interface facts. Do
   not paste source or prompts containing source into this repository or a
   coding agent asked to implement the change.
4. Express the implementation independently, add focused tests from public
   behavior, and request provenance review.
5. Update this document and `NOTICE` when the relationship or distribution
   obligation changes.

When uncertain, stop before importing material and ask the repository owner.
Preserve detailed comparison evidence outside Git; only a sanitized conclusion
belongs in this repository.

## Private native reproduction workflow

`docs/CLEAN_REPRODUCTION.md` inventories the immutable action pins, Rust,
cargo-ndk and NDK inputs, their licenses and redistribution boundaries for the
manual clean-host attempt. The workflow starts from canonical master and
checks out the exact reviewed consumer pin separately from its own tooling.
It never imports the restricted repository, changes the source pin, or grants
publication approval. The GitHub-hosted image's actual version and toolchain
file inventory accompany the evidence; a moving image label alone is not a
reproducibility assertion.

The same technical inventory records the exact official Rust 1.95.0 Apple ARM64
compiler-builtins archive used to classify inherited compiler paths by whole
object identity. Its component license expression is preserved separately from
Rust's general license summary; no archive is vendored by the verifier.

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

## First-party Airspy performance work (2026-09-07)

Kernel and transport additions use only reviewed canonical c927ba987f2b9e1da7a50f1d9debca92d9207447
source plus newly authored safe Rust and synthetic tests. Original worker
revisions 359fcab1b0a2ef93f68b0af66100aab596f85f97 and
9da258863f3ed90bc871e03e5b0cd00ab2715ec1 remain in this same clean lineage.
Cargo.lock and the published nusb 0.2.7 dependency are unchanged. No external
implementation, dependency, fixture or copied graph machinery was introduced.
Reference-source copies used by the CF32 comparator retain MIT OR Apache-2.0
terms and stay in local temporary verification directories. Origins and
physical/timing limits are documented in MACOS_AIRSPY_KERNEL_CPU.md and
MACOS_AIRSPY_TRANSFER_CPU.md. Atomic consumer artifacts must come from one
reviewed integrated source revision; do not mix worker outputs.

## 2026-09-08 packed statistics and resilience integration

First-party safe Rust packed ADC routing/statistics and fixed macOS Airspy
payload profiles extend the reviewed c89f580 baseline. Frozen worker sources
7e50fdd33fa7cbf23add05b96835e5281746aabd and
c1d3a88ea844572fb59e302193b12289e8fc1690 were independently reviewed before
separate integration. The selected 4/4/1 MiB policy preserves 256 KiB transfers
and the bridge; no rate, dependency, nusb, public ABI or QoS change is included.
The packed kernel retains exact DC/filter/output order and per-call statistics.

New source, synthetic tests and diagnostic harnesses are independently authored
under this repository's MIT OR Apache-2.0 terms. No legacy/GPL/research source,
external implementation, private capture or copied graph machinery was used.
Historical and c89 reference copies retain their original licenses and stay in
ignored verification output. Exact worker origins, measured artifacts, recovery
incidents and limits are in MACOS_AIRSPY_PACKED_STATS_CPU.md and
MACOS_AIRSPY_RESILIENCE.md. Atomic consumer outputs require one frozen reviewed
combined revision; no publication or whole-app CPU acceptance is implied.

## Android V4 identity correction (2026-09-09)

Android now offers additive `openUsbDevice`/`nativeOpenByFdWithIdentity` APIs
that preserve actual authorized USB IDs and manufacturer/product metadata.
The existing strict V4 clock predicate is unchanged; generic R828D retains
16 MHz. Existing fd-open callers keep their ABI and behavior. No dependency,
C ABI or radio-register change is included. See
[the implementation and provenance record](ANDROID_USB_IDENTITY.md).
## V4 RF routing observations (2026-09-09)

The V4-specific RF fields use the independently reviewed first-party SondeFox
measurement contract at `d0e201dc13acad9d698ca7f3cad9042e128d018f`, not a new code
upstream. The existing GPL binary was a black-box measurement instrument only;
none of its source, headers or disassembly was read or imported. No dependency
or binary distribution changes. Exact measurement hashes, manufacturer factual
references and source-use limits are in [the RF contract](BLOG_V4_RF_ROUTING.md).

## Android JNI page-size policy

The first-party package-local linker policy follows the official Google Android
and Cargo interface facts linked in [ANDROID_PAGE_SIZE.md](ANDROID_PAGE_SIZE.md).
No provider implementation, dependency, fixture or binary is imported. The
existing NDK/Rust/cargo-ndk pins and their tool/runtime license boundaries remain
as recorded above; the historical fixed-pin reproduction contract is preserved.

The separately named candidate workflow deliberately selects Xcode 27.0/build
27A266a on the standard ARM `xcode-27` image. Its versioned alias, official
image reference and receipt boundaries are in `ANDROID_PAGE_SIZE.md`.
Only tool/interface facts are used; no Apple or Google SDK is redistributed
by this workflow. Its source, hashes and profile do not replace the historical
Xcode 26.6 reconstruction subject.
