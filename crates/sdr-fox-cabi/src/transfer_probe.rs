//! Opt-in, root-operated physical probe. No receiver access occurs in tests.
//! Samples are immediately discarded and output contains only bounded numeric
//! aggregates and explicit RF controls, never descriptors or sample content.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdr_fox_core::{
    ControlRequest, GainRequest, GainStageId, IqFormat, SdrBackend, SdrError, StreamHandle,
    Transport,
};
use sdr_fox_transport::{stream::StreamControl, NusbTransport};

use super::transfer_policy::TransferPolicy;

struct Settings {
    policy: TransferPolicy,
    seconds: u64,
    frequency: u64,
    lna: i32,
    mixer: i32,
    vga: i32,
    lna_agc: bool,
    mixer_agc: bool,
    bias: bool,
}

fn settings(args: impl Iterator<Item = String>) -> Result<Settings, &'static str> {
    let mut args = args.peekable();
    let mut values = BTreeMap::new();
    let mut exclusive = false;
    while let Some(key) = args.next() {
        if key == "--exclusive-hardware" {
            exclusive = true;
        } else {
            if !matches!(
                key.as_str(),
                "--kib"
                    | "--seconds"
                    | "--frequency"
                    | "--rate"
                    | "--lna"
                    | "--mixer"
                    | "--vga"
                    | "--lna-agc"
                    | "--mixer-agc"
                    | "--bias"
            ) {
                return Err(
                    "Unknown option; every RF setting and --exclusive-hardware are required",
                );
            }
            let value = args
                .next()
                .ok_or("Missing option value")?
                .parse::<u64>()
                .map_err(|_| "Expected unsigned integer")?;
            if values.insert(key, value).is_some() {
                return Err("Duplicate option");
            }
        }
    }
    if !exclusive || values.len() != 10 {
        return Err("Require --exclusive-hardware and explicit --kib --seconds --frequency --rate --lna --mixer --vga --lna-agc --mixer-agc --bias");
    }
    let get = |key: &str| values.get(key).copied().ok_or("Missing required option");
    if get("--rate")? != 10_000_000 {
        return Err("Probe requires full 10000000 IQ samples/second");
    }
    let policy =
        TransferPolicy::candidate(usize::try_from(get("--kib")?).map_err(|_| "Invalid KiB")?)
            .ok_or("--kib must be 64, 128 or 256")?;
    let seconds = get("--seconds")?;
    if !(1..=120).contains(&seconds) {
        return Err("--seconds must be 1..120");
    }
    let frequency = get("--frequency")?;
    if frequency == 0 {
        return Err("Frequency must be positive");
    }
    let stage = |key: &str, max| -> Result<i32, &'static str> {
        let value = get(key)?;
        if value > max || value % 10 != 0 {
            return Err("Stage gains must be valid indices times 10");
        }
        i32::try_from(value).map_err(|_| "Invalid gain")
    };
    let flag = |key: &str| -> Result<bool, &'static str> {
        match get(key)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("Boolean controls must be 0 or 1"),
        }
    };
    Ok(Settings {
        policy,
        seconds,
        frequency,
        lna: stage("--lna", 140)?,
        mixer: stage("--mixer", 150)?,
        vga: stage("--vga", 150)?,
        lna_agc: flag("--lna-agc")?,
        mixer_agc: flag("--mixer-agc")?,
        bias: flag("--bias")?,
    })
}

#[derive(Default)]
struct Arrivals {
    count: u64,
    previous: Option<Instant>,
    first: Option<Instant>,
    interval_ns_sum: u128,
    interval_ns_max: u128,
    // Bin 0: <=1us, bin n: <=2^n us, last bin includes larger intervals.
    interval_us_log2: [u64; 32],
}

impl Arrivals {
    fn record(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        if let Some(previous) = self.previous {
            let ns = now.duration_since(previous).as_nanos();
            self.interval_ns_sum += ns;
            self.interval_ns_max = self.interval_ns_max.max(ns);
            let us = u64::try_from(ns.div_ceil(1000)).unwrap_or(u64::MAX).max(1);
            let bin = (64 - (us - 1).leading_zeros()).min(31) as usize;
            self.interval_us_log2[bin] += 1;
        }
        self.previous = Some(now);
        self.count += 1;
    }

    fn json(&self, start: Instant) -> String {
        let first = self.first.map_or("null".to_owned(), |t| {
            t.saturating_duration_since(start).as_nanos().to_string()
        });
        format!("{{\"count\":{},\"first_ns\":{},\"interval_ns_mean\":{},\"interval_ns_max\":{},\"interval_us_log2_bins\":{:?}}}", self.count, first, self.interval_ns_sum / u128::from(self.count.saturating_sub(1).max(1)), self.interval_ns_max, self.interval_us_log2)
    }
}

#[derive(Default)]
struct CompletionStats {
    bytes: u64,
    sizes: BTreeMap<usize, u64>,
    unbucketed_completions: u64,
    arrivals: Arrivals,
}

impl CompletionStats {
    fn record(&mut self, size: usize) {
        self.bytes += size as u64;
        self.arrivals.record(Instant::now());
        if self.sizes.len() < 128 || self.sizes.contains_key(&size) {
            *self.sizes.entry(size).or_default() += 1;
        } else {
            self.unbucketed_completions += 1;
        }
    }

    fn sizes_json(&self) -> String {
        format!(
            "{{{}}}",
            self.sizes
                .iter()
                .map(|(size, count)| format!("\"{size}\":{count}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

#[derive(Clone)]
struct ObservedTransport {
    inner: NusbTransport,
    completion: Arc<Mutex<CompletionStats>>,
    control: Arc<Mutex<Option<StreamControl>>>,
}

impl Transport for ObservedTransport {
    fn control_in(&mut self, request: &ControlRequest) -> Result<Vec<u8>, SdrError> {
        self.inner.control_in(request)
    }
    fn control_out(&mut self, request: &ControlRequest) -> Result<usize, SdrError> {
        self.inner.control_out(request)
    }
    fn bulk_read(&mut self, endpoint: u8, len: usize, timeout: u32) -> Result<Vec<u8>, SdrError> {
        self.inner.bulk_read(endpoint, len, timeout)
    }
    fn boxed_clone(&self) -> Box<dyn Transport> {
        Box::new(self.clone())
    }
    fn start_bulk_stream(
        &mut self,
        endpoint: u8,
        count: usize,
        size: usize,
        depth: usize,
    ) -> Result<StreamHandle, SdrError> {
        let completion = self.completion.clone();
        let stream =
            self.inner
                .start_bulk_stream_observed(endpoint, count, size, depth, move |size| {
                    if let Ok(mut stats) = completion.lock() {
                        stats.record(size);
                    }
                })?;
        *self.control.lock().expect("probe control lock") = Some(stream.control_handle());
        Ok(Box::new(stream))
    }
}

struct StreamGuard(*mut super::SdrFoxStream);
impl Drop for StreamGuard {
    fn drop(&mut self) {
        // SAFETY: this probe registered the token and exclusively owns close.
        unsafe {
            super::sdrfox_close_stream(self.0);
        }
    }
}

fn configure_receiver(
    device: &mut dyn sdr_fox_core::SdrDevice,
    config: &Settings,
) -> Result<u32, &'static str> {
    let rate = device
        .set_sample_rate(10_000_000)
        .map_err(|_| "Sample rate failed")?;
    if rate != 10_000_000 {
        return Err("Applied sample rate differs from full-rate contract");
    }
    device
        .set_frequency(config.frequency)
        .map_err(|_| "Frequency failed")?;
    device
        .set_bias_tee(config.bias)
        .map_err(|_| "Bias tee failed")?;
    // Match SdrFoxMacSource.applyManualGain order; no overall gain/QoS/PPM call.
    device
        .set_stage_agc(GainStageId::Lna, config.lna_agc)
        .map_err(|_| "LNA AGC failed")?;
    device
        .set_stage_agc(GainStageId::Mixer, config.mixer_agc)
        .map_err(|_| "Mixer AGC failed")?;
    for (stage, gain) in [
        (GainStageId::Lna, config.lna),
        (GainStageId::Mixer, config.mixer),
        (GainStageId::Vga, config.vga),
    ] {
        device
            .set_gain(GainRequest::per_stage(stage, gain))
            .map_err(|_| "Stage gain failed")?;
    }
    Ok(rate)
}

/// Run only after the caller has stopped every other receiver owner. Errors
/// are static messages so backend-provided private descriptors cannot leak.
pub fn run(args: impl Iterator<Item = String>) -> Result<(), &'static str> {
    let config = settings(args)?;
    if !cfg!(target_os = "macos") {
        return Err("Physical probe is macOS-only");
    }
    let locations = sdr_fox_transport::enumerate_usb_devices().map_err(|_| "Enumeration failed")?;
    if locations
        .iter()
        .filter(|d| (d.vendor_id, d.product_id) == (0x1d50, 0x60a1))
        .count()
        != 1
    {
        return Err("Require exactly one attached Airspy One receiver");
    }
    let location =
        super::select_location(&locations, 0, super::Kind::Airspy).ok_or("Airspy unavailable")?;
    let desc = super::descriptor_for(&location, sdr_fox_core::DeviceKind::Airspy);
    let completion = Arc::new(Mutex::new(CompletionStats::default()));
    let control = Arc::new(Mutex::new(None));
    let transport = ObservedTransport {
        inner: NusbTransport::open(
            location.vendor_id,
            location.product_id,
            location.match_index,
        )
        .map_err(|_| "Open failed")?,
        completion: completion.clone(),
        control: control.clone(),
    };
    let mut device = super::AirspyBackend
        .open(&desc, Box::new(transport))
        .map_err(|_| "Airspy initialization failed")?;
    let rate = configure_receiver(device.as_mut(), &config)?;
    println!("{{\"event\":\"starting\",\"pid\":{},\"family\":\"airspy_one\",\"rate\":{},\"frequency\":{},\"lna\":{},\"mixer\":{},\"vga\":{},\"lna_agc\":{},\"mixer_agc\":{},\"bias\":{},\"raw_bytes\":{},\"inflight\":{},\"raw_queue_blocks\":{},\"bridge_blocks\":{},\"seconds\":{}}}", std::process::id(), rate, config.frequency, config.lna, config.mixer, config.vga, config.lna_agc, config.mixer_agc, config.bias, config.policy.raw_bytes, config.policy.inflight, config.policy.raw_queue_blocks, config.policy.bridge_blocks, config.seconds);
    let started = Instant::now();
    let stream = device
        .start_stream(config.policy.config(IqFormat::Cf32))
        .map_err(|_| "Start failed")?;
    let adapter = super::SdrFoxStream::with_bridge_depth(stream, true, config.policy.bridge_blocks);
    let handle = super::streams()
        .write()
        .map_err(|_| "Registry unavailable")?
        .insert(adapter)
        .ok_or("Registry exhausted")?;
    let guard = StreamGuard(handle);
    let deadline = started + Duration::from_secs(config.seconds);
    let mut output = vec![0u8; 131_072];
    let mut reads = Arrivals::default();
    let mut timeouts = 0u64;
    let mut errors = 0u64;
    let mut short_reads = 0u64;
    while Instant::now() < deadline {
        // SAFETY: live owned token and writable initialized byte buffer of len.
        let n =
            unsafe { super::sdrfox_read_stream(handle, output.as_mut_ptr(), output.len(), 100) };
        if n < 0 {
            errors += 1;
            break;
        }
        if n == 0 {
            timeouts += 1;
        } else {
            // Keep the caller-buffer writes observable under Release LTO;
            // payload contents still never enter output or persistent storage.
            std::hint::black_box(&output);
            reads.record(Instant::now());
            if n != 131_072 {
                short_reads += 1;
            }
        }
    }
    let receive_elapsed = started.elapsed();
    let mut copy = super::SdrFoxStreamStats::default();
    // SAFETY: live token and stack-owned writable stats output.
    if unsafe { super::sdrfox_stream_stats(handle, &raw mut copy) } != 0 {
        return Err("Adapter stats failed");
    }
    let stop_started = Instant::now();
    drop(guard);
    let stop_ns = stop_started.elapsed().as_nanos();
    let native = control
        .lock()
        .map_err(|_| "Control unavailable")?
        .as_ref()
        .ok_or("No native stream")?
        .stats();
    let completion = completion
        .lock()
        .map_err(|_| "Completion stats unavailable")?;
    println!("{{\"event\":\"complete\",\"elapsed_ns\":{},\"stop_and_join_ns\":{},\"successful_usb_bytes\":{},\"successful_usb_size_histogram\":{},\"histogram_unbucketed\":{},\"usb_arrivals\":{},\"app_read_arrivals\":{},\"read_timeouts_or_eof\":{},\"read_errors\":{},\"short_reads\":{},\"cf32_bytes_read\":{},\"native_blocks_accepted\":{},\"last_sequence\":{},\"last_iq_drop_estimate\":{},\"raw_bytes_delivered\":{},\"raw_pairs_drop_estimate\":{},\"dropped_raw_blocks\":{},\"failed_transfers\":{},\"observed_unknown_overrun_events\":{},\"hardware_loss_counter_available\":false,\"raw_queue_high_water_blocks\":{}}}", receive_elapsed.as_nanos(), stop_ns, completion.bytes, completion.sizes_json(), completion.unbucketed_completions, completion.arrivals.json(started), reads.json(started), timeouts, errors, short_reads, copy.bytes_read, copy.blocks_read, copy.last_sequence, copy.last_dropped, native.bytes_delivered, native.sample_pairs_dropped_estimate, native.dropped_blocks, native.failed_transfers, native.hardware_overruns_unknown, native.high_water_mark);
    if errors > 0 {
        Err("Probe observed read errors")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_controls_fail_before_hardware_access() {
        assert!(settings(std::iter::empty()).is_err());
        let args = "--exclusive-hardware --kib 128 --seconds 30 --frequency 404000000 --rate 10000000 --lna 140 --mixer 150 --vga 150 --lna-agc 0 --mixer-agc 0 --bias 1";
        assert!(settings(args.split_whitespace().map(str::to_owned)).is_ok());
        assert!(settings(
            args.replace("10000000", "2500000")
                .split_whitespace()
                .map(str::to_owned)
        )
        .is_err());
        assert!(settings(
            args.replace("--bias 1", "--bias 2")
                .split_whitespace()
                .map(str::to_owned)
        )
        .is_err());
    }
}
