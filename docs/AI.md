# AI use and AI-assisted contributions

Large language models (LLMs) have been used extensively in the development of
sdr-fox, including implementation, tests, and documentation. This is a
project-wide disclosure; it does not identify which individual lines were
AI-assisted or certify that every behavior has been tested on hardware. The
maintainers are responsible for reviewing and validating what they merge.

The source is public, but external code intake is currently closed. Please
do not submit unsolicited patches. This guide serves invited maintainers now
and external contributors if the agreement and clearance process in
[`CONTRIBUTING.md`](../CONTRIBUTING.md) opens. An accountable covered human must
review and sponsor AI or bot submissions. For permitted changes, we judge the
sources, behavior, tests, and reviewability. An LLM's confident explanation is
not evidence that a device command is correct, a binding is safe, or a license
permits copying.

## Give an LLM the right context

Start with [`CONTRIBUTING.md`](../CONTRIBUTING.md),
[`AGENTS.md`](../AGENTS.md),
[`docs/REPOSITORY_CONTEXT.md`](REPOSITORY_CONTEXT.md),
[`docs/UPSTREAMS.md`](UPSTREAMS.md), and
[`SECURITY.md`](../SECURITY.md). Then choose the relevant map:

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) for crates, traits, and design
  constraints;
- [`docs/INTEGRATION.md`](INTEGRATION.md) for public API and stream contracts;
- [`bindings/android/README.md`](../bindings/android/README.md) for Android
  USB descriptor and connection lifetimes;
- [`crates/sdr-fox-python/README.md`](../crates/sdr-fox-python/README.md) for
  Python builds and the non-hardware binding tests;
- [`bindings/sdr_fox.h`](../bindings/sdr_fox.h) for the generated C surface
  (regenerate it with cbindgen when the C ABI changes).

Give the assistant one bounded task: the observed problem, expected behavior,
affected receiver and platforms, relevant files, and what can be checked
without hardware. Ask it to inspect current signatures and callers before
editing. For a public API change, trace the C, Python, JNI/Kotlin, and CLI
surfaces; for a driver change, trace the mock transport or tuner-bus tests.

## Keep inputs and claims traceable

- Work from the clean SondeFox/sdr-fox `master` lineage. Never ask an LLM to
  recover or copy from the restricted legacy repository, its history, patches,
  or private audit material.
- Use published specifications, datasheets, independent measurements, or
  verified permissive upstreams. Record the exact URL, revision or release,
  license, and the fact used. Do not paste GPL implementation source into a
  prompt for a new implementation. Follow [`docs/UPSTREAMS.md`](UPSTREAMS.md)
  and update notices when a relationship or distribution obligation changes.
- Do not put credentials, precise receive locations, persistent device IDs,
  raw captures, private analysis, or prompt transcripts containing them into
  prompts, issues, fixtures, commits, or logs. Use synthetic data for tests
  whenever possible.
- Separate proposed behavior from observed results. A mock test does not prove
  USB electrical behavior or on-air reception. Record the actual receiver,
  host, sample rate, and duration for any hardware claim, without committing
  the capture.

## Review an AI-assisted change

Have a person review the diff against the task and the source references. Pay
particular attention to unsafe Rust, buffer sizes, cancellation, error paths,
and C/JNI ownership and lifetimes. Check that Android uses its authorized file
descriptor through `nusb` and keeps `UsbDeviceConnection` open until after the
native device closes. Do not accept generated tests that merely restate the
implementation; test observable behavior and failure paths.

For code changes, run the relevant checks from [`AGENTS.md`](../AGENTS.md):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --lib --bins --all-targets -- -D warnings
cargo clippy -p sdr-fox-python --lib -- -D warnings
cargo test --workspace --exclude sdr-fox-python --exclude sdr-fox-jni
cargo test -p sdr-fox-tests
```

Run the binding-specific and hardware checks that the change needs. In the
pull request, say what AI helped with, which sources were used, what a person
checked, the exact commands and results, and what remains unverified. Do not
attach prompt transcripts or private captures. If you cannot verify a claim,
narrow it or state the limit.

This disclosure was inspired by
[UMSH's AI usage note](https://github.com/darconeous/umsh/blob/main/docs/AI.md);
the contribution instructions here describe sdr-fox's own review and source-use
rules.
