// First-party test-only adapter appended equally to frozen and current source.
// Kept out of production; exposes streaming state without altering the kernel.
pub fn oracle_state(s: &IqSynthesizer) -> Vec<u64> {
    let mut values = vec![
        s.dc_average.to_bits() as u64,
        s.mixer_phase as u64,
        s.pending_low_byte.map_or(256, u64::from),
        s.last_raw_samples,
        s.last_clips,
    ];
    for carry in [&s.poly_carry, &s.delay_carry] {
        values.push(carry.len() as u64);
        values.extend(carry.iter().map(|x| x.to_bits() as u64));
    }
    values
}
pub fn oracle_process(s: &mut IqSynthesizer, raw: &[u8], format: usize) -> Vec<u32> {
    match format {
        0 => s
            .synthesize_mini_cf32(raw)
            .iter()
            .map(|x| x.to_bits())
            .collect(),
        1 => s.synthesize_mini(raw).iter().map(|&x| x as u32).collect(),
        2 => s
            .synthesize_mini_cs8(raw)
            .iter()
            .map(|&x| x as u32)
            .collect(),
        3 => s
            .synthesize_mini_cs16(raw)
            .iter()
            .map(|&x| x as u32)
            .collect(),
        4 => explicit_dc::<false>(s, raw)
            .iter()
            .map(|x| x.to_bits())
            .collect(),
        5 => explicit_dc::<true>(s, raw)
            .iter()
            .map(|x| x.to_bits())
            .collect(),
        _ => unreachable!(),
    }
}
fn explicit_dc<const FUSED: bool>(s: &mut IqSynthesizer, raw: &[u8]) -> Vec<f32> {
    s.synthesize_mini_with::<FUSED, _>(raw, 2, |i_vals, q_vals, out| {
        out.extend(
            i_vals
                .iter()
                .zip(q_vals)
                .flat_map(|(&i, &q)| [i.clamp(-1.0, 1.0), q.clamp(-1.0, 1.0)]),
        );
    })
}
