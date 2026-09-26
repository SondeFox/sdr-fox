# Contributing

**External code intake is closed.** The source has MIT OR Apache-2.0 terms,
but the versioned contributor agreements, private agreement register, and
protected clearance check for this repository are not yet operational.
Please do not submit unsolicited code or patches. Invited maintainers may
continue under their separately agreed permissions.

Before external code can be accepted, every author and coauthor must have
current agreement coverage for `SondeFox/sdr-fox`, verified identity and any
required employer/client authority, plus a [DCO 1.1](https://developercertificate.org/)
sign-off for each contribution.
An accountable covered human must review and sponsor AI or bot submissions.
A protected check must bind clearance to the current PR head and authors;
historical contributions need separate review. Agreements and identity records
stay private, never in a PR. No draft agreement is offered for signature by
this notice. See the [source-readiness gate](docs/REPOSITORY_CONTEXT.md#public-release-gate).

## Start here

Read `AGENTS.md`, `docs/REPOSITORY_CONTEXT.md`, `docs/UPSTREAMS.md`, and the
relevant architecture or integration document. Discuss large API, transport,
or licensing changes before implementation.

Create a focused branch from `master`; do not use a `codex/` branch name. Keep
commits reviewable and do not merge or graft history from the restricted
legacy repository.

## Pull requests

The instructions below apply to invited maintainers while external intake is
closed. They will also apply to external contributors after the agreement and
protected-check process is activated.

A pull request should:

- explain the user-visible behavior and the reason for the change;
- list affected platforms, receivers, language bindings, and safety contracts;
- identify every technical reference or new dependency with URL and license;
- include tests that do not require hardware where possible;
- state any hardware validation separately without attaching raw captures; and
- update integration, security, provenance, or notice documentation when the
  corresponding contract changes.

Run formatting, Clippy, workspace tests, cross-crate integration tests, and any
binding-specific checks described in `AGENTS.md`. CI must be green before
merge. Resolve review conversations and use one of the repository's supported
merge methods.

## Data hygiene

Synthetic fixtures are preferred. Never commit credentials, signing material,
precise receive locations, persistent device identifiers, private radio
captures, generated WAV/PNG artifacts, review transcripts, agent state, or
external source corpora.

Report security and provenance concerns privately according to `SECURITY.md`.
