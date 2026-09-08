//! First-party synthetic scheduling tests. No USB or wall-clock stall is used.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use sdr_fox_core::sample::StreamStopHandle;
use sdr_fox_core::{IqSamples, SdrError};
use sdr_fox_transport::stream::{start_stream_concrete, BufferSource};

const RAW_BYTES: usize = 262_144;
const WAIT: Duration = Duration::from_secs(2);

enum Command {
    Deliver(Vec<u8>),
    Stop,
}

struct GatedSource {
    commands: mpsc::Receiver<Command>,
    wake: mpsc::Sender<Command>,
    ready: mpsc::Sender<()>,
    drops: Arc<AtomicUsize>,
}

impl BufferSource for GatedSource {
    fn next_buffer(&mut self) -> Result<Vec<u8>, SdrError> {
        // Reaching the next pull acknowledges that the worker has accounted
        // for the preceding delivery attempt, including a deliberate drop.
        self.ready.send(()).unwrap();
        match self.commands.recv().unwrap() {
            Command::Deliver(bytes) => Ok(bytes),
            Command::Stop => Err(SdrError::Cancelled),
        }
    }

    fn stop_waker(&self) -> Option<StreamStopHandle> {
        let wake = self.wake.clone();
        Some(StreamStopHandle::new(move || {
            let _ = wake.send(Command::Stop);
        }))
    }
}

impl Drop for GatedSource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

fn gated_source() -> (
    GatedSource,
    mpsc::Sender<Command>,
    mpsc::Receiver<()>,
    Arc<AtomicUsize>,
) {
    let (send, commands) = mpsc::channel();
    let (ready, acknowledge) = mpsc::channel();
    let drops = Arc::new(AtomicUsize::new(0));
    (
        GatedSource {
            commands,
            wake: send.clone(),
            ready,
            drops: drops.clone(),
        },
        send,
        acknowledge,
        drops,
    )
}

fn payload(sequence: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|offset| (sequence.wrapping_mul(19) ^ offset.wrapping_mul(73)) as u8)
        .collect()
}

#[test]
fn fixed_raw_queue_budgets_preserve_bytes_and_exact_post_overflow_sequences() {
    for (inflight, depth) in [(4, 8), (16, 16), (32, 16)] {
        let (source, send, acknowledge, drops) = gated_source();
        let mut stream = start_stream_concrete(source, depth, None);
        acknowledge.recv_timeout(WAIT).unwrap();
        for sequence in 0..depth + 3 {
            let len = if sequence == depth + 2 {
                514
            } else {
                RAW_BYTES
            };
            send.send(Command::Deliver(payload(sequence, len))).unwrap();
            acknowledge.recv_timeout(WAIT).unwrap();
        }
        let stats = stream.stats();
        assert_eq!(stats.bytes_delivered, 0);
        assert_eq!(stats.high_water_mark, depth as u64);
        assert_eq!(stats.dropped_blocks, 3);
        assert_eq!(stats.sample_pairs_dropped_estimate, 262_401);
        assert_eq!(stats.failed_transfers, 0);
        assert_eq!(stats.hardware_overruns_unknown, 0);
        assert_eq!(stats.consecutive_errors, 0);
        for sequence in 0..depth {
            let block = stream
                .recv_deadline(Instant::now() + WAIT)
                .unwrap()
                .unwrap();
            assert_eq!(block.sequence, sequence as u64);
            assert_eq!(block.dropped, 262_401);
            let IqSamples::Cu8(actual) = block.samples else {
                panic!("raw bytes expected")
            };
            assert_eq!(actual, payload(sequence, RAW_BYTES), "inflight={inflight}");
        }
        // The producer remains parked until space exists; this survivor proves
        // that all three dropped attempts consumed sequence numbers exactly once.
        send.send(Command::Deliver(payload(depth + 3, RAW_BYTES)))
            .unwrap();
        acknowledge.recv_timeout(WAIT).unwrap();
        let survivor = stream
            .recv_deadline(Instant::now() + WAIT)
            .unwrap()
            .unwrap();
        assert_eq!(survivor.sequence, (depth + 3) as u64);
        assert_eq!(survivor.dropped, 262_401);
        let IqSamples::Cu8(actual) = survivor.samples else {
            panic!("raw bytes expected")
        };
        assert_eq!(actual, payload(depth + 3, RAW_BYTES));
        let stats = stream.stats();
        assert_eq!(stats.bytes_delivered, ((depth + 1) * RAW_BYTES) as u64);
        assert_eq!(stats.dropped_blocks, 3);
        assert_eq!(stats.high_water_mark, depth as u64);
        // This is payload queue capacity only; the synthetic source does not
        // implement the USB ring or claim its bytes are resident.
        assert_eq!(
            stats.high_water_mark as usize * RAW_BYTES,
            if depth == 8 { 2_097_152 } else { 4_194_304 }
        );
        drop(stream);
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }
}

#[test]
fn full_raw_queue_stop_wakes_the_pending_source_and_releases_ownership_once() {
    for depth in [8, 16] {
        let (source, send, acknowledge, drops) = gated_source();
        let stream = start_stream_concrete(source, depth, None);
        acknowledge.recv_timeout(WAIT).unwrap();
        for sequence in 0..depth {
            send.send(Command::Deliver(payload(sequence, RAW_BYTES)))
                .unwrap();
            acknowledge.recv_timeout(WAIT).unwrap();
        }
        let control = stream.control_handle();
        assert_eq!(control.stats().high_water_mark, depth as u64);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        let (finished, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(stream);
            finished.send(()).unwrap();
        });
        completion.recv_timeout(WAIT).unwrap();
        worker.join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 1);
        let stats = control.stats();
        assert_eq!(stats.bytes_delivered, 0);
        assert_eq!(stats.dropped_blocks, 0);
        assert_eq!(stats.failed_transfers, 0);
        assert_eq!(stats.sample_pairs_dropped_estimate, 0);
        assert_eq!(stats.hardware_overruns_unknown, 0);
    }
}
