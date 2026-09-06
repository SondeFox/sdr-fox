# macOS direct USB and Blog V4 clock selection

Source starts at canonical clean-history sdr-fox
`26b6d6c728b7954391a956372012d7253271cf46` (tree
`ea8902c90b37e857be37b6dc3f7963c723bd445a`). No legacy repository, GPL
implementation, or SondeFox research document was consulted. macOS and Android
exclude rusb and libusb1-sys through target tables and module cfgs; other
platforms retain the existing fallback.

## USB backend evidence

Final source uses published, unmodified nusb 0.2.7 (Apache-2.0 OR MIT), crate
archive SHA-256
`18ef13beb3b3a8fc16fd7aea912ebd3d45dde00a9a5b968d0742297468065845`.
The original driver documentation reported macOS asynchronous control-OUT
stalls. A controlled comparison here tested that original nusb control path
against a candidate synchronous IOKit variant. Both streamed the attached
Airspy and Nooelec without 5-second transfer loss/timeouts; the historical
OUT stall was not reproduced. The speculative workaround and vendored nusb
were removed. Do not claim a proved OUT root cause or universal OS/device fix.

## Blog V4 correction and factual sources

The attached Blog V4 opened and set sample rate but failed tuner PLL lock
at 403.2 MHz under both USB control paths. The generic R828D constructor chose
16 MHz. Manufacturer specifications describe a different V4 board clock:

- [RTL-SDR Blog V4 design, 16 August 2023](https://www.rtl-sdr.com/rtl-sdr-blog-v4-dongle-initial-release/),
  “HF Design”: the 28.8 MHz oscillator supplies the tuner and RTL2832U as well
  as the HF mixer. Only that circuit fact is used; no source-code link was read.
- [Manufacturer V4 guide](https://www.rtl-sdr.com/v4/) and
  [quick start guide](https://www.rtl-sdr.com/rtl-sdr-quick-start-guide/): the
  published EEPROM identity uses manufacturer `RTLSDRBlog` and product
  `Blog V4`; drivers depend on retaining those identifiers.

Independent implementation checks VID 0x0bda/PID 0x2838 plus both exact
manufacturer/product strings, after probing an R828D tuner. Only that board
uses the dedicated 28.8 MHz constructor. Missing/different strings, other
VID/PID pairs, other tuner kinds and generic R828D preserve their previous
behavior. Tests cover strict identity and the two reference clocks.

After the correction, that same V4 locked at 403.2 MHz and delivered
20,578,304 CU8 bytes in a five-second window at 2.048 MS/s with zero native
drop count and zero read timeouts. Radio data were discarded; no receiver
identifier, coordinate or capture is retained. This verifies clock programming
and transport only. The manufacturer also documents switched HF/VHF/UHF
inputs/notches; full RF path/sensitivity/HF acceptance is not proved by IQ
bytes and remains a separate test requirement.

Android fd users without manufacturer/product descriptors do not activate this
board-specific path; no Android V4 support improvement is claimed merely from
rebuilding the unchanged JNI binding. The Mac C API preserves the available
native descriptor strings and therefore can identify the attached V4.

## C API and lifetime

The generated `bindings/sdr_fox.h` adds stable receiver enumeration/open;
actual applied rate; queried sample/gain tables; per-stage gain/AGC; tuner
manual mode; IF bandwidth; and reference clock query. Existing APIs remain
compatible. Mac identity uses VID/PID, topology and full manufacturer/product/serial
descriptors. Different models with the same VID/PID and default serial cannot
replace one another on a remembered port. All components use lossless UTF-8
hex, and an identity too large for the bounded C record is excluded rather
than truncated. Receivers indistinguishable by every descriptor still require
manual reselection after detach; descriptor identity cannot prove physical
continuity in that case.
Open holds the exact native USB object and rejects missing/ambiguous identity;
there is no index fallback. Port changes require deliberate reselection.

Original CU8 reads retain RTL bytes; Airspy full-rate CF32 follows its required
real-to-IQ synthesis. Bounded reads own any partially copied block. Coarse
loss counters indicate discontinuity without an invented missing-sample count.
Non-recycling registry handles retain an in-flight operation until completion.

## Verification and reproduction

Rust 1.95.0, cargo-ndk 4.1.2 and NDK 27.2.12479018 were used on arm64 macOS.
Before any change, outgoing Android JNI bytes reproduced exactly:
arm64 `c695325870eae8ffee197da09e2b10bc8741619255406cfae29d1381302bc5f4`,
x86_64 `594cd893d106c65089cd59c8670fbf112c49462037e5fae5eea91afc1de8bc85`.
Final source/binding/Mac/both-JNI hashes, repeated build comparisons, physical
control/stream results and remaining gates belong to the consumer's
`third_party/sdr-fox/MACOS_REPRODUCTION_RECEIPT.md`.

Additional physical checks on the same implementation completed two 30-second
streams per attached receiver with repeated 400/406/403.2 MHz tuning and RTL
1.024/2.4/2.048 MS/s rate cycles. All six windows had zero read timeout and
zero native drop counter. Bias remained off and samples were discarded.
