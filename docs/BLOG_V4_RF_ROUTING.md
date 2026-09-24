# Blog V4 RF routing

The strict `0x0bda:0x2838`, `RTLSDRBlog` / `Blog V4` identity and an R828D
probe select the board implementation. Generic R828D keeps its 16 MHz clock;
R820T/T2 and all other receivers keep their existing programming. No public
C/JNI/Kotlin ABI, dependency, binary or recording format changes here.

## Independently measured inputs

The implementation is authored against canonical source
`afe8c35007460b1bd3dfaa9d73bbbfbe2d29a851` and the sanitized functional contract
`docs/V4_RF_MEASUREMENT_SPEC.md` in the canonical SondeFox consumer at source
`d0e201dc13acad9d698ca7f3cad9042e128d018f`. The first-party diagnostic source is
`b437c1abca441d3091ed476b252c1bbe1a3e9a16`. The independently reviewed contract
covers 57 successful USB observations, 56 distinct frequencies, 12,791 admitted
pre-bulk transfers, and no failed/short controls or dropped trace records.

Private aggregate SHA-256 identities are:

| Measurement set | SHA-256 |
| --- | --- |
| 21 matrix reports | `102119dd62133aa387204e16ba4ce6f58c52499afcd52ed1c58c78000c439913` |
| 25 edge reports | `12432e07c6aa1e537d7150ad3dd36b23bec828bc237186e7cf7efa42239b0ba6` |
| 11 tracking reports | `a6d07d24dddddaf1b4dffeef2db84c1c12be86817f8473747bdde1351f5bb96f` |

The preinstalled black-box `rtl_sdr` measurement subject has SHA-256
`bb79af2d8b320cb8a1965061c407d75475107ef6601c263ce77d8539649c46a5`;
its resolved librtlsdr binary has SHA-256
`1601347f0de58d9f9af829470ff5d463b51b7a1c3b29b10dd98f54a5c61484a0`.
It is a GPL measurement instrument only. No GPL source, headers, disassembly,
reference implementation, raw trace arrays, radio captures, legacy repository,
excluded plugin or research implementation material was consulted or imported
for this change. The proprietary SondeFox tracer/specification is not copied
or redistributed here; only independently reviewed hardware facts and hashes
are recorded. New Rust code and synthetic tests have this repository's
MIT OR Apache-2.0 terms. No new distributed third-party input is introduced.

The manufacturer's [V4 datasheet v1.0](https://www.rtl-sdr.com/wp-content/uploads/2024/12/RTLSDR_V4_Datasheet_V_1_0.pdf)
and [2023 design description](https://www.rtl-sdr.com/rtl-sdr-blog-v4-dongle-initial-release/)
establish the one-SMA triplexer, built-in 28.8 MHz HF conversion, and switched
notch design. These are factual references only; no document asset is bundled.
The measured contract supplies the programming facts missing from those sources.

## Frequency and register contract

Here RF means the frequency physically presented at the receiver's SMA, after
any explicitly configured external converter. The private board plan selects
these fields while preserving every other register bit:

| SMA RF | R05 mask 0x60 | R06 mask 0x08 | GPIO 5 | Tuner RF |
| --- | --- | --- | --- | --- |
| Below 28.8 MHz | 0x20 | 0x08 | Low | SMA RF + 28.8 MHz |
| 28.8 MHz through below 250 MHz | 0x60 | 0x00 | High | SMA RF |
| 250 MHz and above | 0x00 | 0x00 | High | SMA RF |

The exact 28.8 MHz point is a deliberate engineering decision. The reference
instrument selected HF fields there but omitted its HF frequency addition.
This implementation chooses the coherent VHF configuration measured one Hz
above it. That decision is not an exact-boundary RF reception measurement.

R17 mask 0x08 is clear at RF <= 2.2 MHz, 85–112 MHz inclusive, and
172–242 MHz inclusive; it is set elsewhere. This replaces only the ordinary
tracking table's R17 field on V4. All remaining tracking fields still use
LO = tuner RF + current tuner IF. For example, at a 1.815 MHz IF, SMA RF
48.2 MHz yields LO 50.015 MHz and the existing R1B value 0xbe. No table is
imported or changed. Demodulator IF and spectrum inversion remain unchanged.

GPIO 5 uses the existing RTL GPIO read/modify/write helpers to configure its
output and level. Bias GPIO 0 and all unrelated GPIO fields remain independent.
The observed GPIO difference does not establish its electrical purpose or
voltage. No observed whole-register GPIO byte is used as a replacement image.

The device remembers the original user frequency; external SpyVerter translation
still happens once at the device boundary. Bandwidth retunes and reset replay
therefore cannot accumulate either converter offset. Board identity survives
tuner initialization and recovery. Gain controls preserve route masks; PPM
retains the existing demodulator-only behavior. A failed write or PLL lock does
not publish the new requested center; acknowledged shadow writes remain available
for a retry. Partial hardware changes after failure are not rolled back.

## Qualification and remaining scope

Synthetic tests cover measured route/notch edges, the coherent 28.8 MHz decision,
translated LO and tracking boundaries, gain/bandwidth preservation, retry and
initialization, generic behavior, and device GPIO/reset/external-converter paths.
These are programming tests. Exact-artifact RF, Android/app, notch rejection,
HF sensitivity, electrical bias and known on-air decode acceptance remain
separate consumer qualification gates. No historical hardware pass transfers.

Existing live and recorded-IQ conditioning semantics remain unchanged. Their
reference-harmonic enumeration excludes logical harmonic zero; near the bottom
of HF it can omit a post-converter 28.8 MHz leak that would appear near negative
SMA center in baseband. The current empty basis is pass-through and live/replay
remain consistent. This is an identified conditioning limitation, not a measured
leak or a claim of complete HF spur cancellation. Logical RF recording metadata
and decoder/spectrum frequency axes must not become tuner LO values.
