//! Bounded macOS transport policies and fixed Airspy diagnostic baselines.
//! The controlled resilience comparison chose 4/4/1 MiB at 256 KiB with synthesis;
//! see `docs/MACOS_AIRSPY_RESILIENCE.md` for evidence and remaining limits.

use sdr_fox_core::{DeviceKind, IqFormat, StreamConfig};

pub(super) const INFLIGHT_RAW_BYTES: usize = 1_048_576;
pub(super) const QUEUED_RAW_BYTES: usize = 2_097_152;
pub(super) const BRIDGED_CF32_BYTES: usize = 1_048_576;
const PRODUCTION_AIRSPY_KIB: usize = 256;

#[cfg(any(test, feature = "transfer-probe"))]
/// Fixed diagnostic payload selectors cannot override production selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PayloadProfile {
    Baseline,
    Resilience4,
    Resilience8,
}

#[cfg(any(test, feature = "transfer-probe"))]
impl PayloadProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "baseline" => Some(Self::Baseline),
            "4-4-1" => Some(Self::Resilience4),
            "8-4-1" => Some(Self::Resilience8),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Resilience4 => "4-4-1",
            Self::Resilience8 => "8-4-1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TransferPolicy {
    pub raw_bytes: usize,
    pub inflight: usize,
    pub raw_queue_blocks: usize,
    pub bridge_blocks: usize,
}

impl TransferPolicy {
    pub fn candidate(kib: usize) -> Option<Self> {
        if !matches!(kib, 64 | 128 | 256) {
            return None;
        }
        let raw_bytes = kib * 1024;
        Some(Self {
            raw_bytes,
            inflight: INFLIGHT_RAW_BYTES / raw_bytes,
            raw_queue_blocks: QUEUED_RAW_BYTES / raw_bytes,
            // Airspy 16-bit real input at 2x IQ rate becomes CF32 at 2x bytes.
            bridge_blocks: BRIDGED_CF32_BYTES / (raw_bytes * 2),
        })
    }

    /// Root-selected configuration, shared with its physical diagnostic arm.
    /// Keep `candidate()`'s original 1/2/1 MiB budgets as the diagnostic baseline.
    fn resilience4() -> Self {
        Self {
            inflight: 16,
            raw_queue_blocks: 16,
            ..Self::candidate(PRODUCTION_AIRSPY_KIB).expect("reviewed static policy")
        }
    }

    /// Larger profiles are explicitly limited to the reviewed 256 KiB transfer.
    /// No arbitrary byte/count input, environment setting or production override.
    #[cfg(any(test, feature = "transfer-probe"))]
    pub fn diagnostic(kib: usize, profile: PayloadProfile) -> Option<Self> {
        let mut policy = Self::candidate(kib)?;
        match profile {
            PayloadProfile::Baseline => {}
            PayloadProfile::Resilience4 | PayloadProfile::Resilience8 => {
                if kib != PRODUCTION_AIRSPY_KIB {
                    return None;
                }
                policy = Self::resilience4();
                if profile == PayloadProfile::Resilience8 {
                    policy.inflight = 32;
                }
            }
        }
        Some(policy)
    }

    #[cfg(any(test, feature = "transfer-probe"))]
    pub fn payload_bytes(self) -> (usize, usize, usize) {
        (
            self.raw_bytes * self.inflight,
            self.raw_bytes * self.raw_queue_blocks,
            self.raw_bytes * 2 * self.bridge_blocks,
        )
    }

    pub fn config(self, format: IqFormat) -> StreamConfig {
        StreamConfig {
            format,
            buffer_count: self.inflight,
            buffer_size: self.raw_bytes,
            queue_depth: self.raw_queue_blocks,
        }
    }
}

pub(super) fn production_policy(kind: DeviceKind, applied_rate_hz: Option<u32>) -> TransferPolicy {
    policy_for_platform(cfg!(target_os = "macos"), kind, applied_rate_hz)
}

fn policy_for_platform(
    macos: bool,
    kind: DeviceKind,
    applied_rate_hz: Option<u32>,
) -> TransferPolicy {
    if macos && kind == DeviceKind::Airspy {
        TransferPolicy::resilience4()
    } else {
        let config = StreamConfig::default();
        // Whole 64 KiB CU8 transfers take 131 ms at 250 kS/s. Use the
        // consumer Android adapter's existing 16 KiB extent only within
        // RTL's low-rate window. Higher/unknown rates retain their budgets.
        let low_rate_rtl = macos
            && kind == DeviceKind::RtlSdr
            && applied_rate_hz.is_some_and(|hz| (225_001..=300_000).contains(&hz));
        TransferPolicy {
            raw_bytes: if low_rate_rtl {
                16_384
            } else {
                config.buffer_size
            },
            inflight: config.buffer_count,
            raw_queue_blocks: config.queue_depth,
            bridge_blocks: super::BRIDGE_DEPTH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_candidates_keep_the_three_payload_budgets() {
        for kib in [64, 128, 256] {
            let p = TransferPolicy::candidate(kib).unwrap();
            assert_eq!(p.raw_bytes * p.inflight, INFLIGHT_RAW_BYTES);
            assert_eq!(p.raw_bytes * p.raw_queue_blocks, QUEUED_RAW_BYTES);
            assert_eq!(p.raw_bytes * 2 * p.bridge_blocks, BRIDGED_CF32_BYTES);
            assert_eq!((p.raw_bytes * 2) % 131_072, 0);
        }
        for kib in [0, 32, 65, 512, usize::MAX] {
            assert!(TransferPolicy::candidate(kib).is_none());
        }
    }

    #[test]
    fn diagnostic_profiles_have_explicit_fixed_byte_and_count_bounds() {
        for (name, inflight, queued, extra) in [
            ("baseline", 4, 8, 0),
            ("4-4-1", 16, 16, 5_242_880),
            ("8-4-1", 32, 16, 9_437_184),
        ] {
            let profile = PayloadProfile::parse(name).unwrap();
            assert_eq!(profile.name(), name);
            let p = TransferPolicy::diagnostic(256, profile).unwrap();
            assert_eq!(
                (p.raw_bytes, p.inflight, p.raw_queue_blocks, p.bridge_blocks),
                (262_144, inflight, queued, 2)
            );
            let (usb, raw, bridge) = p.payload_bytes();
            assert_eq!(
                (usb, raw, bridge),
                (262_144 * inflight, 262_144 * queued, 1_048_576)
            );
            assert_eq!(usb + raw + bridge, 4_194_304 + extra);
            assert!(usb <= 8_388_608 && raw <= 4_194_304);
            if profile != PayloadProfile::Baseline {
                for kib in [0, 64, 128, 255, 257, 512, usize::MAX] {
                    assert!(TransferPolicy::diagnostic(kib, profile).is_none());
                }
            }
        }
        for value in ["", "4", "16-4-1", "8-8-1", "4-4-2", "BASELINE"] {
            assert!(PayloadProfile::parse(value).is_none());
        }
    }

    #[test]
    fn other_platforms_and_receivers_retain_shared_defaults() {
        for (macos, kind) in [
            (false, DeviceKind::RtlSdr),
            (false, DeviceKind::Airspy),
            (true, DeviceKind::RtlSdr),
        ] {
            let policy = policy_for_platform(macos, kind, None);
            let actual = policy.config(IqFormat::Cf32);
            let expected = StreamConfig::default();
            assert_eq!(actual.buffer_size, expected.buffer_size);
            assert_eq!(actual.buffer_count, expected.buffer_count);
            assert_eq!(actual.queue_depth, expected.queue_depth);
            assert_eq!(policy.bridge_blocks, 8);
        }
        let selected = policy_for_platform(true, DeviceKind::Airspy, None);
        assert_eq!(selected.raw_bytes, 262_144);
        assert_eq!(selected.inflight, 16);
        assert_eq!(selected.raw_queue_blocks, 16);
        assert_eq!(selected.bridge_blocks, 2);
        assert_eq!(
            selected,
            TransferPolicy::diagnostic(256, PayloadProfile::Resilience4).unwrap()
        );
        assert_eq!(selected.payload_bytes(), (4_194_304, 4_194_304, 1_048_576));
        let baseline = TransferPolicy::diagnostic(256, PayloadProfile::Baseline).unwrap();
        assert_eq!(baseline.payload_bytes(), (1_048_576, 2_097_152, 1_048_576));
    }

    #[test]
    fn only_known_macos_rtl_low_rates_select_smaller_transfers() {
        for rate in [
            None,
            Some(0),
            Some(225_000),
            Some(225_001),
            Some(250_000),
            Some(300_000),
            Some(300_001),
            Some(900_001),
            Some(3_200_000),
            Some(u32::MAX),
        ] {
            for macos in [false, true] {
                for kind in [DeviceKind::RtlSdr, DeviceKind::Airspy, DeviceKind::Unknown] {
                    let policy = policy_for_platform(macos, kind, rate);
                    if macos && kind == DeviceKind::Airspy {
                        assert_eq!(policy, TransferPolicy::resilience4());
                    } else {
                        let low = macos
                            && kind == DeviceKind::RtlSdr
                            && matches!(rate, Some(225_001 | 250_000 | 300_000));
                        assert_eq!(
                            policy.raw_bytes,
                            if low { 16_384 } else { 65_536 },
                            "macos={macos}, kind={kind:?}, rate={rate:?}"
                        );
                        assert_eq!(
                            (
                                policy.inflight,
                                policy.raw_queue_blocks,
                                policy.bridge_blocks
                            ),
                            (16, 32, 8)
                        );
                        assert_eq!(policy.raw_bytes % 512, 0);
                    }
                }
            }
        }
        let low = policy_for_platform(true, DeviceKind::RtlSdr, Some(250_000));
        assert_eq!(low.raw_bytes * low.inflight, 262_144);
        assert_eq!(low.raw_bytes * low.raw_queue_blocks, 524_288);
        assert_eq!(low.raw_bytes / 2, 8_192);
    }
}
