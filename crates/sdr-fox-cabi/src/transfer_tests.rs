use super::*;
use sdr_fox_airspy::iq_synth::IqSynthesizer;
use sdr_fox_core::{IqBlock, IqSamples, SdrError};

fn policies() -> Vec<transfer_policy::TransferPolicy> {
    use transfer_policy::{PayloadProfile, TransferPolicy};
    [64, 128, 256]
        .into_iter()
        .map(|kib| TransferPolicy::candidate(kib).unwrap())
        .chain(
            [PayloadProfile::Resilience4, PayloadProfile::Resilience8]
                .into_iter()
                .map(|profile| TransferPolicy::diagnostic(256, profile).unwrap()),
        )
        .collect()
}

fn cf32_block(samples: Vec<f32>, dropped: u64, sequence: u64) -> IqBlock {
    IqBlock {
        samples: IqSamples::Cf32(samples),
        dropped,
        sequence,
        timestamp: None,
        clips: 0,
        raw_samples: 0,
    }
}

#[test]
fn large_raw_and_odd_partitions_preserve_current_synthesis_bits() {
    // Independently generated synthetic containers, including rails and high
    // bits; no recording or external implementation is an input.
    let raw: Vec<u8> = (0..(1_048_576usize + 37))
        .map(|n| (n.wrapping_mul(73) ^ (n >> 3)).to_le_bytes()[0])
        .collect();
    for taps in [45, 47] {
        let synthesize = |partition: &[usize]| {
            let mut synth = IqSynthesizer::with_taps(taps);
            let mut output = Vec::new();
            let mut cursor = 0;
            let mut step = 0;
            let mut clips = 0;
            let mut raw_samples = 0;
            while cursor < raw.len() {
                let end = (cursor + partition[step % partition.len()]).min(raw.len());
                output.extend(
                    synth
                        .synthesize_mini_cf32(&raw[cursor..end])
                        .into_iter()
                        .map(f32::to_bits),
                );
                clips += synth.last_clips();
                raw_samples += synth.last_raw_samples();
                cursor = end;
                step += 1;
            }
            // Complete the retained low byte and exercise state beyond every
            // large call's tail; then compare the actual continued output.
            output.extend(
                synth
                    .synthesize_mini_cf32(&[0x0f, 0x00, 0x00, 0xff, 0x0f])
                    .into_iter()
                    .map(f32::to_bits),
            );
            clips += synth.last_clips();
            raw_samples += synth.last_raw_samples();
            (output, clips, raw_samples)
        };
        let baseline = synthesize(&[65_536]);
        for partition in [
            &[131_072][..],
            &[262_144][..],
            &[1, 131_071, 3, 262_143, 65_535][..],
        ] {
            assert_eq!(
                synthesize(partition),
                baseline,
                "taps={taps}, partition={partition:?}"
            );
        }
    }
}

#[test]
fn app_extent_drains_large_suffixes_with_coherent_metadata_and_errors() {
    for bridge in [false, true] {
        for policy in policies() {
            let (handle, sender, ..) = tests::test_stream_depth(bridge, policy.bridge_blocks);
            let values: Vec<f32> = (0..policy.raw_bytes / 2)
                .map(|n| f32::from_bits(0x3f00_0000 + u32::try_from(n).unwrap()))
                .collect();
            for (sequence, dropped) in [(7, 0), (11, 16_384)] {
                sender
                    .send(Some(Ok(cf32_block(values.clone(), dropped, sequence))))
                    .unwrap();
            }
            sender.send(Some(Err(SdrError::Timeout))).unwrap();
            sender
                .send(Some(Ok(cf32_block(vec![0.25, -0.5], 16_384, 12))))
                .unwrap();
            sender.send(Some(Err(SdrError::DeviceLost))).unwrap();
            let mut bytes = vec![0u8; 131_072];
            let mut stats = SdrFoxStreamStats::default();
            let parts = policy.raw_bytes * 2 / bytes.len();
            for (block_index, (sequence, dropped)) in [(7, 0), (11, 16_384)].into_iter().enumerate()
            {
                for part in 0..parts {
                    assert_eq!(
                        unsafe {
                            sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000)
                        },
                        131_072
                    );
                    let start = part * 32_768;
                    let actual: Vec<u32> = bytes
                        .chunks_exact(4)
                        .map(|v| u32::from_ne_bytes(v.try_into().unwrap()))
                        .collect();
                    assert_eq!(
                        actual,
                        values[start..start + 32_768]
                            .iter()
                            .map(|v| v.to_bits())
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(unsafe { sdrfox_stream_stats(handle, &raw mut stats) }, 0);
                    assert_eq!(stats.last_sequence, sequence);
                    assert_eq!(stats.last_dropped, dropped);
                    assert_eq!(stats.blocks_read, (block_index + 1) as u64);
                    assert_eq!(
                        stats.bytes_read,
                        ((block_index * parts + part + 1) * bytes.len()) as u64
                    );
                }
            }
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000) },
                0
            );
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000) },
                8
            );
            assert_eq!(
                &bytes[..8],
                &[0.25f32.to_ne_bytes(), (-0.5f32).to_ne_bytes()].concat()
            );
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000) },
                -1
            );
            assert_eq!(unsafe { sdrfox_stream_stats(handle, &raw mut stats) }, 0);
            assert_eq!(stats.blocks_read, 3);
            assert_eq!(stats.last_sequence, 12);
            assert_eq!(stats.bytes_read, (policy.raw_bytes * 4 + 8) as u64);
            unsafe {
                sdrfox_close_stream(handle);
            }
        }
    }
}

#[test]
fn cancellation_discards_pending_suffix_and_unblocks_each_saturated_bridge() {
    for bridge in [false, true] {
        for policy in policies() {
            let (handle, sender, stopped, finished, delivered) =
                tests::test_stream_depth(bridge, policy.bridge_blocks);
            for sequence in 0..policy.bridge_blocks + 4 {
                sender
                    .send(Some(Ok(cf32_block(
                        vec![0.5; policy.raw_bytes / 2],
                        0,
                        sequence as u64,
                    ))))
                    .unwrap();
            }
            // Deliberately retain a nonempty suffix even for the baseline size.
            let mut bytes = [0u8; 8];
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000) },
                8
            );
            let expected_delivered = if bridge { policy.bridge_blocks + 2 } else { 1 };
            let deadline = Instant::now() + Duration::from_secs(1);
            while delivered.load(Ordering::Acquire) < expected_delivered
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            assert_eq!(delivered.load(Ordering::Acquire), expected_delivered);
            let started = Instant::now();
            unsafe {
                sdrfox_stop_stream(handle);
            }
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, bytes.as_mut_ptr(), bytes.len(), 1000) },
                0
            );
            unsafe {
                sdrfox_close_stream(handle);
            }
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(stopped.load(Ordering::Acquire));
            assert!(finished.load(Ordering::Acquire));
        }
    }
}

#[test]
fn both_modes_time_out_recover_and_cancel_an_inflight_blocked_read() {
    for bridge in [false, true] {
        for policy in policies() {
            let (handle, sender, stopped, finished, _) =
                tests::test_stream_depth(bridge, policy.bridge_blocks);
            let mut output = vec![0u8; 131_072];
            let before_timeout = Instant::now();
            assert_eq!(
                unsafe { sdrfox_read_stream(handle, output.as_mut_ptr(), output.len(), 5) },
                0
            );
            assert!(before_timeout.elapsed() < Duration::from_secs(1));
            sender
                .send(Some(Ok(cf32_block(vec![0.5; policy.raw_bytes / 2], 0, 0))))
                .unwrap();
            for _ in 0..policy.raw_bytes * 2 / output.len() {
                assert_eq!(
                    unsafe { sdrfox_read_stream(handle, output.as_mut_ptr(), output.len(), 1000) },
                    131_072
                );
            }

            let (done, completion) = std::sync::mpsc::channel();
            let token = handle as usize;
            let reader = thread::spawn(move || {
                let mut bytes = [0u8; 8];
                let n = unsafe {
                    sdrfox_read_stream(
                        token as *mut SdrFoxStream,
                        bytes.as_mut_ptr(),
                        bytes.len(),
                        0,
                    )
                };
                done.send(n).unwrap();
            });
            let entry = resolve_stream(handle).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while !*entry.read_gate.busy.lock().unwrap() && Instant::now() < deadline {
                thread::yield_now();
            }
            assert!(
                *entry.read_gate.busy.lock().unwrap(),
                "reader must own the permit before cancellation"
            );
            drop(entry);
            let before_stop = Instant::now();
            unsafe {
                sdrfox_stop_stream(handle);
            }
            assert_eq!(completion.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
            reader.join().unwrap();
            unsafe {
                sdrfox_close_stream(handle);
            }
            assert!(before_stop.elapsed() < Duration::from_secs(1));
            assert!(stopped.load(Ordering::Acquire));
            assert!(finished.load(Ordering::Acquire));
        }
    }
}
