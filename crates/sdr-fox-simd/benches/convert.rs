//! Throughput benchmark for the cu8 → cf32 conversion.
//!
//! Run with: `cargo bench -p sdr-fox-simd`.
//! Compares the SIMD hot path against the scalar reference on a realistic
//! RTL-SDR block size (262 144 bytes = 131 072 complex samples ≈ one default
//! librtlsdr buffer at 2.4 MS/s).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sdr_fox_simd::{cu8_to_cf32, cu8_to_cf32_scalar, cu8_to_cs16, cu8_to_cs8, Spectrum};

const BLOCK_BYTES: usize = 262_144; // one default librtlsdr async buffer

fn bench_convert(c: &mut Criterion) {
    let input: Vec<u8> = (0..BLOCK_BYTES).map(|i| (i % 256) as u8).collect();
    let mut output = vec![0.0f32; BLOCK_BYTES];

    let mut group = c.benchmark_group("cu8_to_cf32");
    group.throughput(Throughput::Bytes(BLOCK_BYTES as u64));

    group.bench_function("simd_dispatched", |b| {
        b.iter(|| {
            cu8_to_cf32(black_box(&input), black_box(&mut output));
        });
    });

    // Public scalar backend: benchmark exactly the implementation used as the
    // correctness oracle instead of a transcribed loop that can drift.
    group.bench_function("scalar_reference", |b| {
        b.iter(|| {
            cu8_to_cf32_scalar(black_box(&input), black_box(&mut output));
            black_box(&output);
        });
    });

    group.finish();
}

fn bench_integer_formats(c: &mut Criterion) {
    let input: Vec<u8> = (0..BLOCK_BYTES).map(|i| (i % 256) as u8).collect();
    let mut group = c.benchmark_group("cu8_integer_formats_with_allocation");
    group.throughput(Throughput::Bytes(BLOCK_BYTES as u64));

    // Include allocation in both sides. This exposes whether zero-initializing
    // then filling a helper output is actually cheaper than direct collection.
    group.bench_function("cs8_direct_collect", |b| {
        b.iter(|| {
            let output: Vec<i8> = black_box(&input)
                .iter()
                .map(|&byte| i8::from_ne_bytes([byte ^ 0x80]))
                .collect();
            black_box(output);
        });
    });
    group.bench_function("cs8_zero_then_helper", |b| {
        b.iter(|| {
            let mut output = vec![0i8; input.len()];
            cu8_to_cs8(black_box(&input), black_box(&mut output));
            black_box(output);
        });
    });
    group.bench_function("cs16_direct_collect", |b| {
        b.iter(|| {
            let output: Vec<i16> = black_box(&input)
                .iter()
                .map(|&byte| (i16::from(byte) - 128) << 8)
                .collect();
            black_box(output);
        });
    });
    group.bench_function("cs16_zero_then_helper", |b| {
        b.iter(|| {
            let mut output = vec![0i16; input.len()];
            cu8_to_cs16(black_box(&input), black_box(&mut output));
            black_box(output);
        });
    });
    group.finish();
}

fn bench_spectrum(c: &mut Criterion) {
    // 512-bin spectrum over 512 complex samples — typical waterfall column.
    const BINS: usize = 512;
    let mut spec = Spectrum::new(BINS);
    let iq: Vec<f32> = (0..BINS * 2).map(|i| ((i as f32) / 10.0).sin()).collect();

    c.benchmark_group("spectrum_fft_512")
        .throughput(Throughput::Elements(BINS as u64))
        .bench_function("compute_power_dbfs", |b| {
            b.iter(|| {
                black_box(spec.compute_power_dbfs(black_box(&iq)));
            });
        });
}

criterion_group!(
    benches,
    bench_convert,
    bench_integer_formats,
    bench_spectrum
);
criterion_main!(benches);
