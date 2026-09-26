# Security policy

## Supported code

The current `master` branch is the supported source for security fixes. Source
availability does not make a CI artifact or an application binary a supported
release. Any supported binary versions must be identified separately in a
published release; none is announced here.

## Report a vulnerability privately

Do not open a public issue. Use GitHub's **Report a vulnerability** flow in the
Security tab when it is available. Otherwise contact an organization owner
through an already trusted private channel and ask for a secure handoff.

Include the affected revision, platform and receiver, impact, reproduction
steps, and a minimal proof of concept. Remove credentials, precise field
locations, persistent device identifiers, and unrelated sample data. If a
capture is essential, arrange a private transfer rather than committing or
attaching it to an issue.

Please allow time to reproduce and coordinate a fix before disclosure. The
maintainers will acknowledge receipt, establish a private tracking plan, and
credit reporters who want attribution when the fix can be disclosed.

## Sensitive areas

Extra review is required for:

- unsafe Rust, raw pointers, JNI and C ABI lifetime/ownership boundaries;
- USB descriptors, transfer lengths, cancellation, and device reset paths;
- parsing or DSP code that consumes untrusted input;
- CI workflows, dependency updates, release artifacts, and generated bindings;
- Android USB permission and file-descriptor ownership; and
- provenance or licensing findings that could affect distribution.

CI and tests must not require repository secrets. Never add access tokens,
signing material, private captures, or real user/device data to source,
fixtures, Actions logs, or artifacts.
