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
