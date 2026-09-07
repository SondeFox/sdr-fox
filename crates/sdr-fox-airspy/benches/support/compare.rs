mod candidate;
mod reference;
use std::hint::black_box;
use std::time::Instant;

fn raw_stream(n: usize, pattern: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| {
            let v = match pattern {
                0 => ((i.wrapping_mul(977) + i / 7) & 4095) as u16,
                1 => {
                    if i % 2 == 0 {
                        0
                    } else {
                        4095
                    }
                }
                2 => 4095,
                3 => ((i / 64) & 4095) as u16,
                4 => {
                    if i % 8 == 0 {
                        12
                    } else {
                        4083
                    }
                }
                5 => [0, 4, 5, 2048, 4091, 4092, 4095, 65535][i % 8],
                _ => (2048.0 + 1700.0 * (i as f32 * 1.4533273).sin()).round() as u16,
            };
            v.to_le_bytes()
        })
        .collect()
}
fn parity() {
    let mut calls = 0;
    for taps in [5, 7, 9, 31, 45, 47, 49, 63] {
        for format in 0..6 {
            for pattern in 0..7 {
                let raw = raw_stream(1031, pattern);
                for stride in [1, 2, 3, 4, 7, 13, 31, 32, 33, 61, 127, 257, 4096] {
                    let mut old = reference::IqSynthesizer::with_taps(taps);
                    let mut new = candidate::IqSynthesizer::with_taps(taps);
                    for (i, block) in raw.chunks(stride).enumerate() {
                        assert_eq!(reference::oracle_process(&mut old, block, format),
                            candidate::oracle_process(&mut new, block, format),
                            "output taps={taps} format={format} pattern={pattern} stride={stride} call={i}");
                        assert_eq!(reference::oracle_state(&old), candidate::oracle_state(&new),
                            "state taps={taps} format={format} pattern={pattern} stride={stride} call={i}");
                        calls += 1;
                        // Empty calls are valid except while a low byte is pending:
                        // preserve the existing behavior, including that case.
                        if i % 37 == 0 {
                            assert_eq!(
                                reference::oracle_process(&mut old, &[], format),
                                candidate::oracle_process(&mut new, &[], format)
                            );
                            assert_eq!(
                                reference::oracle_state(&old),
                                candidate::oracle_state(&new)
                            );
                            calls += 1;
                        }
                    }
                    old.reset();
                    new.reset();
                    assert_eq!(reference::oracle_state(&old), candidate::oracle_state(&new));
                    assert_eq!(
                        reference::oracle_process(&mut old, &raw, format),
                        candidate::oracle_process(&mut new, &raw, format)
                    );
                    assert_eq!(reference::oracle_state(&old), candidate::oracle_state(&new));
                }
            }
        }
    }
    // Long adversarial streams exercise recurrence state and production block sizes.
    for pattern in 0..7 {
        let raw = raw_stream(262_145, pattern);
        for stride in [65_536, 131_072, 262_144, 65_537] {
            let mut old = reference::IqSynthesizer::new();
            let mut new = candidate::IqSynthesizer::new();
            for block in raw.chunks(stride) {
                assert_eq!(
                    reference::oracle_process(&mut old, block, 0),
                    candidate::oracle_process(&mut new, block, 0)
                );
                assert_eq!(reference::oracle_state(&old), candidate::oracle_state(&new));
                calls += 1;
            }
        }
    }
    println!("{{\"parity\":\"passed\",\"compared_calls\":{calls},\"formats\":6,\"tap_counts\":8,\"patterns\":7}}");
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    parity();
    if args.iter().any(|x| x == "--verify-only") {
        return;
    }
    let pairs = args
        .get(1)
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(7);
    let iterations = args
        .get(2)
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(100);
    for containers in [32_768, 65_536, 131_072] {
        let raw = raw_stream(containers, 0);
        let mut old = reference::IqSynthesizer::new();
        let mut new = candidate::IqSynthesizer::new();
        for _ in 0..20 {
            black_box(old.synthesize_mini_cf32(black_box(&raw)));
            black_box(new.synthesize_mini_cf32(black_box(&raw)));
        }
        let mut old_times = Vec::new();
        let mut new_times = Vec::new();
        for pair in 0..pairs {
            let mut measured = [0.0; 2];
            for index in 0..2 {
                let side = (pair + index) % 2;
                let started = Instant::now();
                for _ in 0..iterations {
                    if side == 0 {
                        black_box(old.synthesize_mini_cf32(black_box(&raw)));
                    } else {
                        black_box(new.synthesize_mini_cf32(black_box(&raw)));
                    }
                }
                measured[side] = started.elapsed().as_nanos() as f64 / iterations as f64;
            }
            old_times.push(measured[0]);
            new_times.push(measured[1]);
            println!("{{\"containers\":{containers},\"pair\":{pair},\"reference_ns\":{},\"candidate_ns\":{},\"ratio\":{}}}", measured[0], measured[1], measured[1]/measured[0]);
        }
        old_times.sort_by(f64::total_cmp);
        new_times.sort_by(f64::total_cmp);
        println!("{{\"containers\":{containers},\"reference_median_ns\":{},\"candidate_median_ns\":{},\"median_ratio\":{}}}",old_times[pairs/2],new_times[pairs/2],new_times[pairs/2]/old_times[pairs/2]);
    }
}
