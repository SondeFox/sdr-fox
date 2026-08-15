use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sdr_fox_dsp::demods::SsbDemag;
use sdr_fox_dsp::filters::{ComplexLowPass, LowPass};
use sdr_fox_dsp::{adsb, FmDemod, PolyphaseChannelizer};

fn deterministic_iq(complex_samples: usize) -> Vec<f32> {
    (0..complex_samples)
        .flat_map(|index| {
            let x = index as f32;
            [(x * 0.017).sin(), (x * 0.013).cos()]
        })
        .collect()
}

fn bench_dsp(criterion: &mut Criterion) {
    const COMPLEX_SAMPLES: usize = 32_768;
    let iq = deterministic_iq(COMPLEX_SAMPLES);
    let real: Vec<f32> = iq.chunks_exact(2).map(|pair| pair[0]).collect();

    let mut group = criterion.benchmark_group("streaming_dsp");
    group.throughput(Throughput::Elements(COMPLEX_SAMPLES as u64));

    let mut low_pass = LowPass::new(15_000.0, 2_400_000.0, 50, 401);
    group.bench_function("low_pass_dec50_401", |bencher| {
        bencher.iter(|| black_box(low_pass.process(black_box(&real))));
    });

    let mut complex_low_pass = ComplexLowPass::new(90_000.0, 2_400_000.0, 10, 81);
    group.bench_function("complex_low_pass_dec10_81", |bencher| {
        bencher.iter(|| black_box(complex_low_pass.process(black_box(&iq))));
    });

    let mut fm = FmDemod::new(2_400_000.0, 48_000.0, 180_000.0);
    group.bench_function("fm_demod_2m4_to_48k", |bencher| {
        bencher.iter(|| black_box(fm.process(black_box(&iq))));
    });

    let mut ssb = SsbDemag::new();
    group.bench_function("ssb_hilbert_255", |bencher| {
        bencher.iter(|| black_box(ssb.process(black_box(&iq), true)));
    });

    let adsb_input: Vec<u8> = (0..COMPLEX_SAMPLES * 2)
        .map(|index| ((index * 73 + index / 11) & 0xff) as u8)
        .collect();
    group.bench_function("adsb_decode_cu8", |bencher| {
        bencher.iter(|| black_box(adsb::decode_cu8(black_box(&adsb_input))));
    });

    let mut channelizer = PolyphaseChannelizer::new(256, 8).unwrap();
    let mut channels = Vec::with_capacity(iq.len());
    group.bench_function("pfb_256x8", |bencher| {
        bencher.iter(|| {
            channels.clear();
            black_box(
                channelizer
                    .process(black_box(&iq), black_box(&mut channels))
                    .unwrap(),
            )
        });
    });
    group.finish();
}

criterion_group!(benches, bench_dsp);
criterion_main!(benches);
