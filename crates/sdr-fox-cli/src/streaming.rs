//! Shared receive, framing, cancellation, and raw-output helpers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sdr_fox_core::{IqBlock, IqSamples, SdrError, StreamSink};

const RECEIVE_POLL: Duration = Duration::from_millis(100);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SIGNAL_HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

/// Install the process-wide Ctrl-C handler once.
pub fn install_signal_handler() -> Result<(), String> {
    SIGNAL_HANDLER
        .get_or_init(|| {
            ctrlc::set_handler(|| SHUTDOWN.store(true, Ordering::Release))
                .map_err(|error| format!("install Ctrl-C handler: {error}"))
        })
        .clone()
}

/// Process-wide graceful-shutdown flag.
#[must_use]
pub fn shutdown_flag() -> &'static AtomicBool {
    &SHUTDOWN
}

/// Add a user-controlled duration without allowing `Instant` overflow.
pub fn checked_deadline(start: Instant, duration: Duration) -> Result<Instant, String> {
    start
        .checked_add(duration)
        .ok_or_else(|| "requested duration is too large for this platform".into())
}

/// Apply a requested sample rate and reject both a zero request and a broken
/// backend response that reports a zero negotiated rate.
pub fn negotiate_sample_rate(
    requested: u32,
    set_rate: impl FnOnce(u32) -> Result<u32, SdrError>,
) -> Result<u32, String> {
    if requested == 0 {
        return Err("sample rate must be greater than zero".into());
    }
    let actual =
        set_rate(requested).map_err(|error| format!("set sample rate {requested}: {error}"))?;
    if actual == 0 {
        return Err(format!(
            "device returned an invalid zero sample rate for request {requested}"
        ));
    }
    Ok(actual)
}

/// Receive one block, polling often enough to observe Ctrl-C.
///
/// A timed wait is not an end-of-stream condition. Fatal stream errors are
/// returned, and an unexplained channel close is also an error. `Ok(None)` is
/// reserved for the requested deadline or shutdown flag.
pub fn recv_until(
    stream: &mut dyn StreamSink,
    deadline: Option<Instant>,
    shutdown: &AtomicBool,
) -> Result<Option<IqBlock>, String> {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(None);
        }
        let now = Instant::now();
        if deadline.is_some_and(|limit| now >= limit) {
            return Ok(None);
        }
        let poll = now.checked_add(RECEIVE_POLL).unwrap_or(now);
        let poll_deadline = deadline.map_or(poll, |limit| limit.min(poll));
        match stream.recv_deadline(poll_deadline) {
            Some(Ok(block)) => return Ok(Some(block)),
            Some(Err(SdrError::Timeout)) => {}
            Some(Err(error)) => return Err(error.to_string()),
            None if end_was_requested(deadline, shutdown, Instant::now()) => return Ok(None),
            None => return Err("stream ended before the requested capture completed".into()),
        }
    }
}

fn end_was_requested(deadline: Option<Instant>, shutdown: &AtomicBool, now: Instant) -> bool {
    shutdown.load(Ordering::Acquire) || deadline.is_some_and(|limit| now >= limit)
}

/// Borrow or serialize one IQ block in the documented raw-file byte order.
///
/// Integer and floating-point multi-byte values are always little-endian;
/// `scratch` is reused by callers across blocks.
pub fn sample_bytes<'a>(samples: &'a IqSamples, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    scratch.clear();
    match samples {
        IqSamples::Cu8(bytes) => bytes,
        IqSamples::Cs8(values) => {
            scratch.extend(values.iter().map(|value| value.to_ne_bytes()[0]));
            scratch
        }
        IqSamples::Cs16(values) => {
            scratch.reserve(values.len().saturating_mul(2));
            for value in values {
                scratch.extend_from_slice(&value.to_le_bytes());
            }
            scratch
        }
        IqSamples::Cf32(values) => {
            scratch.reserve(values.len().saturating_mul(4));
            for value in values {
                scratch.extend_from_slice(&value.to_le_bytes());
            }
            scratch
        }
    }
}

/// Chunk-invariant frame assembler. Full frames already contiguous in a
/// caller block are borrowed directly; only a boundary-spanning frame and the
/// final tail use owned scratch storage.
pub struct FrameAssembler {
    frame_len: usize,
    hop_len: usize,
    tail: Vec<f32>,
    scratch: Vec<f32>,
}

impl FrameAssembler {
    /// Create an assembler for `frame_len` floats and a `hop_len`-float step.
    pub fn new(frame_len: usize, hop_len: usize) -> Result<Self, String> {
        if frame_len == 0 || hop_len == 0 || hop_len > frame_len {
            return Err("frame and hop lengths must satisfy 0 < hop <= frame".into());
        }
        Ok(Self {
            frame_len,
            hop_len,
            tail: Vec::with_capacity(frame_len),
            scratch: Vec::with_capacity(frame_len),
        })
    }

    /// Consume every complete frame made available by `input`.
    pub fn push(&mut self, input: &[f32], mut consume: impl FnMut(&[f32])) {
        let tail_len = self.tail.len();
        let virtual_len = tail_len.saturating_add(input.len());
        let mut start = 0usize;
        while start
            .checked_add(self.frame_len)
            .is_some_and(|end| end <= virtual_len)
        {
            if start >= tail_len {
                let input_start = start - tail_len;
                consume(&input[input_start..input_start + self.frame_len]);
            } else {
                self.scratch.clear();
                self.scratch.extend_from_slice(&self.tail[start..]);
                let needed = self.frame_len - self.scratch.len();
                self.scratch.extend_from_slice(&input[..needed]);
                consume(&self.scratch);
            }
            start += self.hop_len;
        }

        if start < tail_len {
            self.tail.copy_within(start..tail_len, 0);
            self.tail.truncate(tail_len - start);
            self.tail.extend_from_slice(input);
        } else {
            let input_start = start - tail_len;
            self.tail.clear();
            self.tail.extend_from_slice(&input[input_start..]);
        }
        debug_assert!(self.tail.len() < self.frame_len);
    }

    /// Discard any partial frame while retaining allocated scratch capacity.
    pub fn clear(&mut self) {
        self.tail.clear();
        self.scratch.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use sdr_fox_core::{sample::StreamStopHandle, IqFormat};

    use super::*;

    struct ScriptedSink {
        results: VecDeque<Option<Result<IqBlock, SdrError>>>,
    }

    impl StreamSink for ScriptedSink {
        fn recv(&mut self) -> Option<Result<IqBlock, SdrError>> {
            self.results.pop_front().flatten()
        }

        fn recv_deadline(&mut self, _deadline: Instant) -> Option<Result<IqBlock, SdrError>> {
            self.results.pop_front().flatten()
        }

        fn stop_handle(&self) -> StreamStopHandle {
            StreamStopHandle::new(|| {})
        }
    }

    fn block() -> IqBlock {
        IqBlock {
            samples: IqSamples::Cu8(vec![1, 2]),
            dropped: 0,
            sequence: 0,
            timestamp: None,
            clips: 0,
            raw_samples: 0,
        }
    }

    #[test]
    fn timeout_does_not_hide_later_fatal_error() {
        let mut sink = ScriptedSink {
            results: [
                Some(Err(SdrError::Timeout)),
                Some(Err(SdrError::DeviceLost)),
            ]
            .into(),
        };
        let shutdown = AtomicBool::new(false);
        let error = recv_until(&mut sink, None, &shutdown).unwrap_err();
        assert!(error.contains("device lost"));
    }

    #[test]
    fn timeout_then_data_keeps_stream_usable() {
        let mut sink = ScriptedSink {
            results: [Some(Err(SdrError::Timeout)), Some(Ok(block()))].into(),
        };
        let shutdown = AtomicBool::new(false);
        assert!(recv_until(&mut sink, None, &shutdown).unwrap().is_some());
    }

    #[test]
    fn unexplained_end_is_not_clean_eof() {
        let mut sink = ScriptedSink {
            results: [None].into(),
        };
        let shutdown = AtomicBool::new(false);
        assert!(recv_until(&mut sink, None, &shutdown).is_err());
    }

    #[test]
    fn requested_shutdown_is_clean() {
        let mut sink = ScriptedSink {
            results: VecDeque::new(),
        };
        let shutdown = AtomicBool::new(true);
        assert!(recv_until(&mut sink, None, &shutdown).unwrap().is_none());
    }

    #[test]
    fn channel_end_at_the_deadline_is_clean() {
        let shutdown = AtomicBool::new(false);
        let deadline = Instant::now();
        assert!(end_was_requested(Some(deadline), &shutdown, deadline));
        assert!(!end_was_requested(
            Some(deadline + Duration::from_millis(1)),
            &shutdown,
            deadline
        ));
    }

    #[test]
    fn raw_serialization_covers_every_format() {
        let mut scratch = Vec::new();
        assert_eq!(
            sample_bytes(&IqSamples::Cu8(vec![1, 255]), &mut scratch),
            [1, 255]
        );
        assert_eq!(
            sample_bytes(&IqSamples::Cs8(vec![-1, 127]), &mut scratch),
            [255, 127]
        );
        assert_eq!(
            sample_bytes(&IqSamples::Cs16(vec![0x1234, -2]), &mut scratch),
            [0x34, 0x12, 0xfe, 0xff]
        );
        assert_eq!(
            sample_bytes(&IqSamples::Cf32(vec![1.0]), &mut scratch),
            1.0f32.to_le_bytes()
        );
        assert_eq!(IqFormat::Cu8, IqSamples::Cu8(Vec::new()).format());
    }

    fn frames(chunks: &[&[f32]], frame: usize, hop: usize) -> Vec<Vec<f32>> {
        let mut assembler = FrameAssembler::new(frame, hop).unwrap();
        let mut out = Vec::new();
        for chunk in chunks {
            assembler.push(chunk, |frame| out.push(frame.to_vec()));
        }
        out
    }

    #[test]
    fn frame_assembly_is_chunk_invariant_without_overlap() {
        let input = (0..37).map(|value| value as f32).collect::<Vec<_>>();
        let expected = frames(&[&input], 8, 8);
        for split in 0..=input.len() {
            assert_eq!(frames(&[&input[..split], &input[split..]], 8, 8), expected);
        }
    }

    #[test]
    fn frame_assembly_is_chunk_invariant_at_half_overlap() {
        let input = (0..37).map(|value| value as f32).collect::<Vec<_>>();
        let expected = frames(&[&input], 8, 4);
        for split in 0..=input.len() {
            assert_eq!(frames(&[&input[..split], &input[split..]], 8, 4), expected);
        }
        assert_eq!(expected[1], input[4..12]);
    }

    #[test]
    fn frame_assembly_is_invariant_across_many_small_chunks() {
        let input = (0..97).map(|value| value as f32).collect::<Vec<_>>();
        let expected = frames(&[&input], 16, 8);
        let chunks = input.chunks(3).collect::<Vec<_>>();
        assert_eq!(frames(&chunks, 16, 8), expected);
    }

    #[test]
    fn stop_handle_test_type_is_sendable() {
        let handle = Arc::new(AtomicBool::new(false));
        let cloned = Arc::clone(&handle);
        StreamStopHandle::new(move || cloned.store(true, Ordering::Release)).stop();
        assert!(handle.load(Ordering::Acquire));
    }
}
