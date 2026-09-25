# Android JNI page layout

`sdr-fox-jni` declares its Android linker policy in its package-local
`build.rs`. It supplies both `-Wl,-z,max-page-size=16384` and
`-Wl,-z,common-page-size=16384` to the Android cdylib link. The first aligns
`PT_LOAD` segments; the second also controls the GNU_RELRO boundary with the
reviewed NDK r27 toolchain. A 16 KB LOAD alignment alone is insufficient.

The build script reads `CARGO_CFG_TARGET_OS`, so cross-compilation from a Mac
still selects Android correctly. Non-Android targets receive neither flag.
The arguments do not change global RUSTFLAGS, the Mac C ABI build, Cargo.lock,
JNI declarations, USB ownership or radio behavior. No new dependency is used.

The current policy follows Google's [16 KB page-size guidance](https://developer.android.com/guide/practices/page-sizes)
and Cargo's [cdylib link-argument contract](https://doc.rust-lang.org/cargo/reference/build-scripts.html#rustc-link-arg-cdylib),
checked on 2026-09-24. These are interface facts, not copied implementation.

## Build and inspect

Keep the pinned Rust 1.95.0, cargo-ndk 4.1.2, NDK 27.2.12479018 and the
source/Cargo/HOME remap order documented in
[`bindings/android/README.md`](../bindings/android/README.md). Build both
`arm64-v8a` and `x86_64` with explicit API 21 and locked dependencies. Inspect
the verbose rustc/linker observations and each actual shared library; merely
finding the flags in this script is not artifact evidence.

For every native library in the final app, verify ELF64/machine identity,
16 KB LOAD alignment and offset/address congruence, and
`(GNU_RELRO.VirtAddr + GNU_RELRO.MemSiz) % 16384 == 0` when RELRO is present.
RELRO describes a protection range: its FileSiz can exceed MemSiz. Do not
apply the `PT_LOAD` FileSiz-versus-MemSiz constraint to RELRO.
Recheck JNI exports, Android system dependencies, no-libusb target graphs and
private-path rejection. Google's signed APK also needs the trusted SDK's
`zipalign -c -P 16 -v 4` inspection; never rewrite its signed bytes.

Run the target-selection regression using the pinned compiler on PATH:

```sh
python3 -m unittest discover -s scripts/android/tests -p 'test_*.py'
```

It compiles and executes the real build script for Android, other target OS
values and absent metadata. It creates only temporary first-party test
executables, uses no receiver and makes no radio or signing claim. The
repository's native formatting, clippy and test gates still apply.

## Runtime and reproduction boundaries

This integration combines canonical runtime source
`fb34d8c600725b54c5a950234c892a593c343968` with maintained master tooling from
`f87e42f27780feda4040e8024bbd07b60390302c`. The merge preserves the shipped
Cargo.lock, bindings and C ABI. It also retains master's existing equivalent
`IqSynthesizer::reset` array-fill cleanup for the Rust 1.95 clippy gate.
No restricted history is imported.

The old `clean-reproduction.yml`, `scripts/reproduction/expected.json` and
fixed-source reconstruction remain evidence for their exact historical
fb34d8c bytes. They do not validate a new page-layout candidate. A consumer
refresh requires a newly reviewed source revision and the complete same-source
Mac archive, generated C header, Kotlin binding and both JNI libraries,
independent artifact inspection and fresh reconstruction evidence. Even if a
component's bytes remain unchanged, retain its new build identity.

Static layout and a local build do not prove complete app compatibility.
Run the final app in a 16 KB environment (confirm `getconf PAGE_SIZE` returns
`16384`) and exercise native decoding, maps, receiver inputs and lifecycle.
Hardware, signed-app, provider, rights and public-distribution gates remain
separate. The source change neither publishes nor replaces a consumer binary.

## Fixed candidate reconstruction

The manual **Android 16 KB host-safe candidate reconstruction** workflow uses
reviewed master tooling and fixed runtime
`2d25727523646c166771f066634b16f60ce22977`, tree
`021353fabc37dca936cede81b4ce54808c4e5c83`. It selects the authored
`android16kb-2d257275-hostsafe` profile in both the builder and independent verifier.
Callers cannot supply a source revision, expected manifest or tool path.
`scripts/reproduction/android-page-size-hostsafe-expected.json` freezes all five
inspected local reference outputs; it is a comparison target, not evidence
that a hosted reconstruction, consumer refresh or hardware test has passed.

The candidate uses standard ARM `xcode-27` with the versioned
`/Applications/Xcode_27.0.app` alias and requires Xcode 27.0/build 27A266a.
The alias may resolve to Apple's release-candidate-named bundle; containment
uses the resolved directory and acceptance requires the exact reported build.
Rust 1.95.0, cargo-ndk 4.1.2, NDK 27.2.12479018/API 21, Mac deployment target
14.0 and the existing remaps remain pinned. The official
[runner image inventory](https://github.com/actions/runner-images/blob/main/images/macos/xcode-27-arm64-Readme.md)
documents the image; the actual image/tool identities are retained per run.

The same guarded builder creates fresh hosted tool/cache/target state,
regenerates the header and runs the C smoke. Candidate verification adds final
JNI linker-argument and actual LOAD/RELRO checks. Reports bind the selected
manifest's original-byte digest and candidate identity, including failures;
standalone verification still cannot establish freshness. The private job is
manual, read-only, bounded to 45 minutes and retains evidence for seven days.
There are no signing, publication or automatic vendoring steps.

Candidate authority is restricted to canonical repository ID `1334845447`
(`R_kgDOT5AgBw`) and owner `h3lix1`, user ID `18344733`
(`MDQ6VXNlcjE4MzQ0NzMz`). An early job condition checks the repository and
initial/triggering owner context. Before creating build evidence or moving
caches/installing tools, the candidate host guard reads the canonical repository
and exact current run-attempt metadata. Both initial and triggering actor IDs,
node IDs, user type and names must match, including on reruns; the repository
must remain private, unarchived and default to master. Run ID/attempt, manual
event, workflow path and tooling revision must match the active job. Missing,
contradictory or unavailable metadata rejects the attempt.

Only the build step receives the existing read-only job token, with `contents`
and `actions` read permissions for these metadata requests. Redirects and
oversized/ambiguous JSON are refused. A fixed, token-free authority projection
is bound into the build receipt and checked against its run identity; the token
never enters the build subprocess environment, receipt or command log.
GitHub documents [run-attempt metadata](https://docs.github.com/en/rest/actions/workflow-runs#get-a-workflow-run-attempt)
and [initial/triggering contexts](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts).
This authority check applies only to the new candidate profile.

Omitting `--profile` preserves historical current-pin reconstruction. Its
workflow, fb34d8c expected bytes and Xcode 26.6 contract remain unchanged.
The original `android16kb-2d257275` profile and
`android-page-size-expected.json` likewise retain their initial command/byte
contract. Neither prior manifest is replaced by the host-safe reference.
Historical fb34d8c outputs are not subjected to the candidate layout policy.
The lightweight guard/profile/inspection regressions run in the existing
rustfmt CI job and require no NDK, hardware, secrets or extra runner job.

## Host build stripping

Exact runtime source 2d still has `[profile.release] strip = "symbols"`.
On the macOS 27 hosted image the original candidate stopped at Rust E0463
loading `thiserror_impl`; the failed host's underlying dyld error was not
retained. A local stripped proc-macro had a string-pool offset misaligned to
eight bytes. This supports, but does not prove, an OS-specific loader rejection.

The new profile adds only Cargo's documented host build-dependency override:

```sh
cargo +1.95.0 build \
  --config 'profile.release.build-override.strip="none"' \
  -vv --locked --offline --release --target aarch64-apple-darwin -p sdr-fox-cabi
```

Use the same explicit `--config` after `build` in the cargo-ndk command in
the Android README. Keep the reviewed source/Cargo/HOME remaps, existing
Rust/NDK pins and `MACOSX_DEPLOYMENT_TARGET=14.0`. The recipe does not change
runtime Cargo.toml or general release stripping. Official Cargo documentation
describes [build overrides](https://doc.rust-lang.org/cargo/reference/profiles.html#build-dependencies)
and [command-line configuration](https://doc.rust-lang.org/cargo/reference/config.html#command-line-overrides).

Verification binds the exact new Mac/Android command, the receipt's policy,
the effective unstripped host proc-macro/build-script commands, and
`strip=symbols` on actual Mac and JNI runtime commands. The host-safe Mac
compiler is also explicitly bound to the pinned inventoried rustc. Plain
release commands remain distinct; local success is not macOS 27 hosted proof.

Changing the host profile also changes target crate identities. The measured
Mac archive has 21 changed object payloads and a net 32-byte size increase;
this does not mean only 32 bytes differ. Both JNI hashes change as well.
The new manifest therefore records a separate five-output reference rather
than relabeling earlier bytes. Header/Kotlin bytes and C/JNI declarations
remain unchanged. Local C-link/run, ABI, package and page-layout checks do not
replace independent hosted reconstruction or final app/device acceptance.
