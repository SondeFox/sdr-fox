# Current consumer pin: clean native reproduction

This manual workflow attempts one cold, privately retained reconstruction of
the consumer's complete Mac/header/Kotlin/both-Android-JNI compatibility set.
It does not publish source, adopt binaries, sign an app, approve provenance,
qualify hardware, change billing limits, or create a self-hosted runner.

## Separate workflow and runtime revisions

The workflow was introduced from canonical `master`
`26b6d6c728b7954391a956372012d7253271cf46`. Its orchestration checkout is the
reviewed workflow commit. Its separate source checkout is always canonical
`fb34d8c600725b54c5a950234c892a593c343968`, tree
`fb5fcf5ce09c8c31bb4004b5861d4533c37b581d`, rooted at
`7bcb45cd2a993240abe7f41dcaeec4a84bc04aeb`. This does not merge that runtime
branch into master or change the consumer pin. Canonical history alone is
fetched; the legacy archive is never an input.

The expected manifest's `reference_consumer` preserves the original measurement
in application repository ID `1334845456`, commit
`2b5e999bd9a2b552c3b84347fd3583e35a9c12d0`, tree
`680db2183af9fb2ad054e720f905eadf377dc146`, and its original URL. After the
application cutover, that private repository is retained as
`SondeFox/SondeFox-private-archive-20260924`; its stable
[repository ID lookup](https://api.github.com/repositories/1334845456)
identifies the archive independently of its name. The replacement application
repository ID `1384652189` retains cleaned application history. The original
measurement remains attributed to its recorded repository, commit and tree;
the replacement alone establishes no new native acceptance.

After normal review and merge into private canonical master, an authorized
maintainer can manually run **Current-pin clean native reproduction** from
master. It has no push, PR, schedule, matrix, retry, signing, or publication
trigger. It uses one standard `macos-26` GitHub-hosted arm64 VM, read-only
repository permissions, no credentials in the checked-out Git configuration,
and a 45-minute job limit. The build driver limits its work to 38 minutes to
leave time for verification and evidence upload. Failed attempts remain failed.

## Cold boundary and inputs

The driver refuses ordinary local execution before touching caches. On the
guarded disposable runner it preserves HOME, quarantines any default Cargo
registry/git cache outside the evidence directory, creates a new Rust home,
and installs Rust 1.95.0 and its three target standard libraries. cargo-ndk
4.1.2 is built with `--locked` in a separate bootstrap Cargo home. No Actions
cache or previous target output is reused. Production builds leave CARGO_HOME
unset, populate the default HOME/.cargo from the exact lockfile, then run
locked and offline. Cached compiler/dependency outputs are not used as proof.

NDK 27.2.12479018 is newly installed and its source.properties, clang, and LLD
hashes are checked against the historical reviewed toolchain receipt. Android
uses explicit API 21. Mac uses Xcode 26.6 and deployment target 14.0; installed
OS, image, SDK and tool identities are recorded rather than assumed identical
to the historical builder. An unavailable pinned tool or unexpected setting
fails the attempt instead of selecting a replacement.

The exact RUSTFLAGS order is source root to `/workspace/sdr-fox`, default Cargo
home to `/cargo`, then HOME to `/home/builder`. The final rule intentionally
produces `/home/builder/.cargo` in panic paths. Setting CARGO_HOME=/cargo changes
bytes. The driver removes the disposable source's generated header before the
Mac build and requires exact regeneration; cbindgen's fallback to an existing
header cannot masquerade as successful generation.

The build also records actual Darwin and selected Apple clang/nm identities,
executable hashes and bounded tool output. Android `-vv` diagnostics bind each
JNI rustc invocation to cargo-ndk's observed linker, clang target and API-level
assignments. A pinned-clang `-###` check independently resolves those observed
arguments to API-21 cc1 triples, startup objects and system library directories.
Missing or contradictory observations fail; the configured `--platform 21`
argument alone is not accepted as effective-link evidence. No linker wrapper,
compiler flag affecting native bytes, or arbitrary environment dump is added.

## Exact provenance of prebuilt compiler object paths

The Mac archive can contain upstream compiler build paths whose prefix happens
to equal the hosted runner's HOME. The verifier distinguishes these only by
complete object provenance. It still rejects the actual source, Cargo-home and
isolated Rustup-home prefixes everywhere, before considering any exception.
For Mac AR HOME matches alone, a strict archive parser requires every occurrence
to lie inside a native member whose name, payload length, full SHA-256 and
multiplicity match the following exact reference. Headers, symbol tables,
modified/spoofed/extra members, malformed numeric fields or padding, and
unaccounted occurrences fail. Android and text artifacts receive no exception.

The reference is read from the validated Rust `1.95.0-aarch64-apple-darwin`
sysroot, never from a workflow input or a build receipt. The selected rustc
binary must match SHA-256
`b829b733131d4e1673eeebd1f34d06ae1e9ff4977b051313cf42e2a9e79ecf1c`, its pinned
version/host, and that sysroot. A missing or changed reference fails closed.
No reference binary is vendored or added to the five output artifacts.

| Immutable input | Identity |
| --- | --- |
| [Official Rust standard-library distribution](https://static.rust-lang.org/dist/2026-04-16/rust-std-1.95.0-aarch64-apple-darwin.tar.xz) | 27,317,176 bytes; SHA-256 `9b30089b0f767cb91b2190ffec55a9beeb2a21a1405d8da0f664d7e09d08e6d8` |
| Sysroot `lib/rustlib/aarch64-apple-darwin/lib/libcompiler_builtins-da5ac53f4a183f75.rlib` | 3,291,616 bytes; SHA-256 `10c965331110c1c53556eab71fe6db80f199a71c1f746d68eafba96f4ac841cb` |
| Rust compiler source | `59807616e1fa2540724bfbac14d7976d7e4a3860` (Rust 1.95.0) |

The official distribution was checksum-verified before reading its reference
member and license files. Its root `LICENSE-MIT`, `LICENSE-APACHE`, and
`COPYRIGHT` have SHA-256 values respectively
`b71bd43a069ca0641a9ecfe585ca7b3c53b5cc1608f8b68321168698e28b5ea1`,
`62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a`, and
`172020dbfd5b53a226dfde77616190a48dcff519b0bc0e6deb91a8450782c4af`.
Rust's [copyright policy at the exact source revision](https://github.com/rust-lang/rust/blob/59807616e1fa2540724bfbac14d7976d7e4a3860/COPYRIGHT)
retains component-specific exceptions to its general MIT/Apache terms.
The pinned compiler-builtins 0.1.160
[package declaration](https://github.com/rust-lang/rust/blob/59807616e1fa2540724bfbac14d7976d7e4a3860/library/compiler-builtins/compiler-builtins/Cargo.toml)
specifies `MIT AND Apache-2.0 WITH LLVM-exception AND (MIT OR Apache-2.0)`.
Its [license text](https://github.com/rust-lang/rust/blob/59807616e1fa2540724bfbac14d7976d7e4a3860/library/compiler-builtins/LICENSE.txt)
has SHA-256 `ab6eec6caf0fa5775e411c7a8bc6a45c4ef2956b0980b157ab74fc5cd62a928b`.
Retain the applicable notices and license texts when redistributing existing
runtime portions; the LLVM exception has its own conditions and is not a
blanket exception for redistribution of the entire reference archive.
This classifier reads an existing toolchain input and grants no new rights or
application distribution approval.

A read-only analysis of retained run `35972312923` found all 343 HOME-prefix
occurrences in 143 exact reference members. Its Mac artifact remains
`fe062c4f7727cba40ff43ee09e3a8b7741e7f8f68718b56f1bf795e9b5144e6f`.
The report counts inherited compiler paths instead of claiming there are none.
The failed run, absent Android outputs and full cold-reconstruction gate remain
unchanged; this analysis alone is not a successful reconstruction.

## Evidence and independent acceptance

The build driver writes `build-receipt.json`, bounded command logs, a Rust
toolchain hash inventory, target package graphs, and these five outputs under
`artifacts/`:

- `libsdr_fox_ffi.a`, `sdr_fox.h`, `SdrFox.kt`
- `arm64-v8a/libsdr_fox_jni.so`, `x86_64/libsdr_fox_jni.so`

The separate `verify_current_pin.py` compares exact reviewed hashes, sizes,
source/lock identities, complete target package sets, declarations and actual
native exports, architectures, dynamic dependencies, and tool constraints. It
does not trust a build-produced assertion of success. A partial or mismatched
set fails. An ABI match never converts a byte mismatch into reproduction.

A first-party C smoke program must link the produced archive/generated header
with IOKit, CoreFoundation, iconv and libSystem, then call only `sdrfox_version()`
and print exactly `0.1.0`. Link failure, run failure, or a wrong result fails the
proof. Its source/executable and output hashes, exit codes, input hashes and
command-log references are bound in the build receipt. The temporary executable
stays outside the five-artifact directory and remains available to the separate
verifier during the job; it is never an application release or hardware test.
First-party C source for this smoke and the Android dry-run probe is generated
from the reviewed driver; no third-party implementation or fixture is copied.
Structured command paths use stable role names; raw command observations remain
only in the bounded private logs. The independent verifier receives the explicit
build receipt and checks these observations as well as the five binary subjects.

The private artifact is retained for seven days, including failure evidence
where available. A hard cancellation/runner loss can prevent upload. No Cargo
cache, SDK, whole home, signing material, capture, or credential is uploaded.
An independent reviewer must download and rehash successful outputs against
the current consumer before adding a sanitized receipt there. Nothing is
automatically vendored, released, or used to replace expected hashes.

Run guard/verifier unit tests with Python 3.11+ before merge:

```sh
python3 -m unittest discover -s scripts/reproduction/tests -p 'test_*.py'
```

These tests validate orchestration, failure behavior, and verification; they
are not evidence of a native build. Existing Rust CI remains separate.

## Tool origins and redistribution

First-party Python, workflow, tests and this document were independently
authored under the repository's MIT OR Apache-2.0 terms using the canonical
source/consumer receipts and official tool documentation. No runtime source,
dependency lock, generated native artifact or radio fixture was imported.

| Tool | Immutable version/origin | License and distribution boundary |
|---|---|---|
| checkout action | [actions/checkout, 3d3c42e5aac5ba805825da76410c181273ba90b1](https://github.com/actions/checkout/tree/3d3c42e5aac5ba805825da76410c181273ba90b1) | MIT, GitHub and contributors; CI execution only; preserve its included copyright/license if redistributed |
| artifact action | [actions/upload-artifact, 043fb46d1a93c77aae656e7c1c64a875d1fc6a0a](https://github.com/actions/upload-artifact/tree/043fb46d1a93c77aae656e7c1c64a875d1fc6a0a) | MIT, GitHub and contributors; CI execution only; preserve its included copyright/license if redistributed |
| Rust toolchain | [Rust 1.95.0](https://static.rust-lang.org/dist/channel-rust-1.95.0.toml), rustc 59807616e, cargo f2d3ce0bd | MIT OR Apache-2.0 with bundled component notices including LLVM terms; toolchain is not uploaded; resulting static runtime follows consumer notices |
| cargo-ndk | [4.1.2 crate archive](https://crates.io/api/v1/crates/cargo-ndk/4.1.2/download), SHA-256 `903cc87cda6ab7a2ff82a74065e1c2ae5baa869546b32eb4aacabcc6ed5a670f` | MIT OR Apache-2.0 and locked transitive tool dependencies; bootstrap tool is not redistributed |
| Android NDK | [Google r27c / 27.2.12479018](https://developer.android.com/ndk/downloads), exact installed tool hashes in driver/expected requirements | Android SDK license agreement and included third-party terms; build tool only, not uploaded |
| Hosted image | [GitHub macos-26 arm64 image](https://github.com/actions/runner-images/blob/main/images/macos/macos-26-arm64-Readme.md), actual ImageVersion recorded per run; Xcode 26.6 selected | GitHub Actions service and Apple SDK terms; image/tools are not redistributed; preinstalled Python/rustup/sdkmanager bootstrap identities recorded |

The hosted-image label is maintained by GitHub rather than an immutable VM
image; the receipt binds the actual image version and downloaded toolchain
inventory. Exact output equality, not the label, determines reproduction.

At planning time a standard Mac runner costs $0.062/minute beyond included
minutes: a 45-minute attempt is approximately $2.79 of runner compute, excluding
ordinary review CI, storage, and tax. This is an estimate, not a spend-limit
change. See [GitHub's current rates](https://docs.github.com/en/billing/reference/actions-runner-pricing).
Standard hosted runners are free for properly public repositories; clearance
must precede a visibility change regardless of CI cost.
