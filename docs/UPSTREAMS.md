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
