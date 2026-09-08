//! Internal transport candidates. Root's controlled physical comparison chose
//! 256 KiB with the existing synthesis bridge; see the transfer CPU receipt.

use sdr_fox_core::{IqFormat, StreamConfig};

pub(super) const INFLIGHT_RAW_BYTES: usize = 1_048_576;
pub(super) const QUEUED_RAW_BYTES: usize = 2_097_152;
pub(super) const BRIDGED_CF32_BYTES: usize = 1_048_576;
const PRODUCTION_AIRSPY_KIB: usize = 256;

#[derive(Clone, Copy, Debug)]
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

    pub fn config(self, format: IqFormat) -> StreamConfig {
        StreamConfig {
            format,
            buffer_count: self.inflight,
            buffer_size: self.raw_bytes,
            queue_depth: self.raw_queue_blocks,
        }
    }
}

pub(super) fn production_policy(airspy: bool) -> TransferPolicy {
    policy_for_platform(cfg!(target_os = "macos"), airspy)
}

fn policy_for_platform(macos: bool, airspy: bool) -> TransferPolicy {
    if macos && airspy {
        TransferPolicy::candidate(PRODUCTION_AIRSPY_KIB).expect("reviewed static policy")
    } else {
        let config = StreamConfig::default();
        TransferPolicy {
            raw_bytes: config.buffer_size,
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
    fn other_platforms_and_receivers_retain_shared_defaults() {
        for (macos, airspy) in [(false, false), (false, true), (true, false)] {
            let policy = policy_for_platform(macos, airspy);
            let actual = policy.config(IqFormat::Cf32);
            let expected = StreamConfig::default();
            assert_eq!(actual.buffer_size, expected.buffer_size);
            assert_eq!(actual.buffer_count, expected.buffer_count);
            assert_eq!(actual.queue_depth, expected.queue_depth);
            assert_eq!(policy.bridge_blocks, 8);
        }
        let selected = policy_for_platform(true, true);
        assert_eq!(selected.raw_bytes, 262_144);
        assert_eq!(selected.inflight, 4);
        assert_eq!(selected.raw_queue_blocks, 8);
        assert_eq!(selected.bridge_blocks, 2);
    }
}
