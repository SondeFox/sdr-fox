use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use sdr_fox_airspy::iq_synth::IqSynthesizer;

fn bench_iq_synth(criterion: &mut Criterion) {
    const CONTAINERS: usize = 65_536;
    let raw: Vec<u8> = (0..CONTAINERS)
        .flat_map(|index| {
            let value = ((index * 977 + index / 7) & 0x0fff) as u16;
            value.to_le_bytes()
        })
        .collect();
    let mut synth = IqSynthesizer::new();
    let mut group = criterion.benchmark_group("airspy_iq_synth");
    group.throughput(Throughput::Elements(CONTAINERS as u64));
    group.bench_function("cu8_47tap", |bencher| {
        bencher.iter(|| black_box(synth.synthesize_mini(black_box(&raw))));
    });
    group.finish();
}

criterion_group!(benches, bench_iq_synth);
criterion_main!(benches);
