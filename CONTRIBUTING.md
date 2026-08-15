# Contributing

sdr-fox is currently developed in a private, invite-only incubation period.
Thanks for helping make the radio stack reliable and safe before its eventual
public release.

## Start here

Read `AGENTS.md`, `docs/REPOSITORY_CONTEXT.md`, `docs/UPSTREAMS.md`, and the
relevant architecture or integration document. Discuss large API, transport,
or licensing changes before implementation.

Create a focused branch from `master`; do not use a `codex/` branch name. Keep
commits reviewable and do not merge or graft history from the restricted
legacy repository.

## Pull requests

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
