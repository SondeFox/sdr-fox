# Provenance review summary

**Review date:** 2026-08-04  
**Snapshot posture:** post-remediation, private incubation  
**Purpose:** engineering provenance record; not legal advice

## Outcome

An engineering review compared the developing sdr-fox implementation with 38
public SDR implementations, including 13 under copyleft licenses. The review
found no evidence of a wholesale port or copied source file. It did identify a
small number of narrow expression-level similarities and one attribution gap.

Those findings were addressed before this clean snapshot was prepared:

- register-block documentation was rewritten in project-specific language;
- internal tuner-range fields were renamed rather than retaining upstream
  naming choices;
- an unused fixed-point arctangent helper was removed;
- nearest-gain selection was independently expressed with `abs_diff`;
- comments that overstated or misstated upstream relationships were corrected;
- MIT attribution and hardware-protocol acknowledgements were added to
  [`NOTICE`](NOTICE).

All of those code remediations are present in this source snapshot. The full
comparison corpus, side-by-side findings, working notes, generated evidence,
and legacy Git history are intentionally not part of this repository.

## Review method

The review combined several mechanical checks with targeted human inspection:

- substantive verbatim-comment comparison;
- normalized structural-line overlap;
- token fingerprinting with unrelated Rust projects as controls;
- direct review of the highest-ranked file pairs; and
- manual inspection of identifiers, documentation, constants, and algorithms
  that normalization can obscure.

Mechanical similarity scores alone were not treated as a legal conclusion.
Hardware register values, bit fields, and mandated protocol sequences were
considered separately from discretionary source expression.

## Upstream posture

The project acknowledges the Osmocom rtl-sdr community as a source of public
hardware-interface knowledge and acknowledges permissively licensed projects
where a design pattern or default informed the implementation. These
acknowledgements are retained for transparency. See
[`docs/UPSTREAMS.md`](docs/UPSTREAMS.md) for the operational rules future
maintainers and coding agents must follow.

No legacy branch, tag, pull-request ref, or commit is an approved upstream for
this repository. Do not merge, rebase, cherry-pick, or graft history from the
restricted development archive. Future upstream updates must be reviewed and
re-expressed as new work against the clean repository.

## Remaining gate

This review supports engineering decisions; it does not determine copyright
scope or replace counsel. Before making the repository public or distributing
binaries, the owner must complete the open-source release checklist, review
third-party notices, and obtain any legal sign-off they consider necessary.

If a future review finds a provenance concern, report it privately according
to [`SECURITY.md`](SECURITY.md), preserve the evidence outside the repository,
and remediate it in a new commit without importing the questioned source or
its history.

## 2026-09-06 macOS integration

The C API extensions and Mac transport selection are independently written
against canonical clean source. Final macOS uses unmodified nusb 0.2.7 with
rusb/libusb target-excluded. A candidate vendor workaround was removed after
physical comparison failed to reproduce the historic async OUT fault.

The Blog V4 tuner clock correction uses the manufacturer's published design
and device identification instructions, linked in docs/MACOS_USB.md. No GPL
source implementation or quarantined legacy source was consulted for it.
Generic R828D behavior remains at the prior 16 MHz reference. Physical tests
established that the V4 changed from PLL failure to IQ streaming at UHF; RF
sensitivity and complete V4 switched-filter/HF support are not established.

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
