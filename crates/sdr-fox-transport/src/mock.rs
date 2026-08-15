//! Mock transport for unit tests.
//!
//! Records control requests and replays scripted responses. This is the
//! backbone that lets every driver crate (`sdr-fox-rtlsdr`, `sdr-fox-airspy`)
//! be unit-tested without real hardware: the test scripts the register-read
//! replies (e.g. the R820T2 probe returning `0x69` at addr `0x00`) and the
//! test asserts on the recorded control-write sequence.

use std::collections::VecDeque;

use sdr_fox_core::{ControlRequest, SdrError, StreamHandle, TransferDirection, Transport};

use crate::stream::{start_stream, validate_stream_config, SyntheticSource};

/// A scripted reply for one control request. `direction` and `request`/`value`/
/// `index` select which request this reply serves; `payload` is returned for
/// IN requests (ignored for OUT).
#[derive(Debug, Clone)]
pub struct ScriptedReply {
    /// Direction this reply applies to.
    pub direction: TransferDirection,
    /// `bRequest` to match (`None` = wildcard).
    pub request: Option<u8>,
    /// `wValue` to match (`None` = wildcard).
    pub value: Option<u16>,
    /// `wIndex` to match (`None` = wildcard).
    pub index: Option<u16>,
    /// Payload returned for IN transfers.
    pub payload: Vec<u8>,
}

impl ScriptedReply {
    /// A wildcard IN reply (matches any IN control request).
    #[must_use]
    pub fn any_in(payload: Vec<u8>) -> Self {
        Self {
            direction: TransferDirection::In,
            request: None,
            value: None,
            index: None,
            payload,
        }
    }

    /// A wildcard OUT acknowledgement (matches any OUT control request).
    #[must_use]
    pub fn any_out() -> Self {
        Self {
            direction: TransferDirection::Out,
            request: None,
            value: None,
            index: None,
            payload: Vec::new(),
        }
    }
}

/// One recorded control request, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// Direction.
    pub direction: TransferDirection,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// Payload (for OUT) or expected length (for IN).
    pub data: Vec<u8>,
}

/// A transport that records requests and replays scripted replies.
///
/// Replies are consumed FIFO. If no scripted reply matches a request, the mock
/// returns a zero-filled payload of the requested length for IN transfers and
/// `Ok(data.len())` for OUT transfers. Set [`MockTransport::strict`] to make
/// unmatched requests return an error instead.
pub struct MockTransport {
    in_replies: VecDeque<ScriptedReply>,
    out_replies: VecDeque<ScriptedReply>,
    recorded: Vec<RecordedRequest>,
    strict: bool,
    /// Bulk reads return this buffer sliced by length.
    bulk_data: Vec<u8>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    /// Construct an empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_replies: VecDeque::new(),
            out_replies: VecDeque::new(),
            recorded: Vec::new(),
            strict: false,
            bulk_data: Vec::new(),
        }
    }

    /// Push a scripted reply.
    pub fn push_reply(&mut self, reply: ScriptedReply) -> &mut Self {
        self.replies_for_mut(reply.direction).push_back(reply);
        self
    }

    /// Queue a sequence of replies.
    pub fn extend_replies(
        &mut self,
        replies: impl IntoIterator<Item = ScriptedReply>,
    ) -> &mut Self {
        for reply in replies {
            self.push_reply(reply);
        }
        self
    }

    /// In strict mode, unmatched requests error instead of returning zeros.
    pub fn strict(&mut self, strict: bool) -> &mut Self {
        self.strict = strict;
        self
    }

    /// Set the data returned by [`Transport::bulk_read`].
    pub fn set_bulk_data(&mut self, data: Vec<u8>) -> &mut Self {
        self.bulk_data = data;
        self
    }

    /// Borrow the recorded control requests, in arrival order.
    #[must_use]
    pub fn recorded(&self) -> &[RecordedRequest] {
        &self.recorded
    }

    /// Take ownership of the recorded requests.
    #[must_use]
    pub fn take_recorded(&mut self) -> Vec<RecordedRequest> {
        std::mem::take(&mut self.recorded)
    }

    /// Count recorded requests matching the predicate.
    #[must_use]
    pub fn count_matching(&self, f: impl Fn(&RecordedRequest) -> bool) -> usize {
        self.recorded.iter().filter(|r| f(r)).count()
    }

    fn find_reply(
        &mut self,
        req: &ControlRequest,
        direction: TransferDirection,
    ) -> Option<ScriptedReply> {
        let replies = self.replies_for_mut(direction);
        let pos = replies.iter().position(|r| {
            r.request.map_or(true, |x| x == req.request)
                && r.value.map_or(true, |x| x == req.value)
                && r.index.map_or(true, |x| x == req.index)
        })?;
        replies.remove(pos)
    }

    fn replies_for_mut(&mut self, direction: TransferDirection) -> &mut VecDeque<ScriptedReply> {
        match direction {
            TransferDirection::In => &mut self.in_replies,
            TransferDirection::Out => &mut self.out_replies,
        }
    }
}

impl Transport for MockTransport {
    fn control_in(&mut self, req: &ControlRequest) -> Result<Vec<u8>, SdrError> {
        debug_assert_eq!(req.direction, TransferDirection::In);
        self.recorded.push(RecordedRequest {
            direction: TransferDirection::In,
            request: req.request,
            value: req.value,
            index: req.index,
            data: req.data.clone(),
        });
        if let Some(reply) = self.find_reply(req, TransferDirection::In) {
            if reply.payload.len() != req.data.len() {
                return Err(SdrError::ShortTransfer {
                    operation: "control_in",
                    expected: req.data.len(),
                    actual: reply.payload.len(),
                });
            }
            return Ok(reply.payload);
        }
        if self.strict {
            return Err(SdrError::Transport(format!(
                "strict mock: no scripted reply for IN req={} val={} idx={}",
                req.request, req.value, req.index
            )));
        }
        // Default: return zeros of the requested length.
        Ok(vec![0u8; req.data.len()])
    }

    fn control_out(&mut self, req: &ControlRequest) -> Result<usize, SdrError> {
        debug_assert_eq!(req.direction, TransferDirection::Out);
        let n = req.data.len();
        self.recorded.push(RecordedRequest {
            direction: TransferDirection::Out,
            request: req.request,
            value: req.value,
            index: req.index,
            data: req.data.clone(),
        });
        if self.find_reply(req, TransferDirection::Out).is_some() {
            return Ok(n);
        }
        if self.strict {
            return Err(SdrError::Transport(format!(
                "strict mock: no scripted reply for OUT req={} val={} idx={}",
                req.request, req.value, req.index
            )));
        }
        Ok(n)
    }

    fn bulk_read(
        &mut self,
        _endpoint: u8,
        len: usize,
        _timeout_ms: u32,
    ) -> Result<Vec<u8>, SdrError> {
        let take = len.min(self.bulk_data.len());
        Ok(self.bulk_data[..take].to_vec())
    }

    fn start_bulk_stream(
        &mut self,
        _endpoint: u8,
        buffer_count: usize,
        buffer_size: usize,
        queue_depth: usize,
    ) -> Result<StreamHandle, SdrError> {
        validate_stream_config(buffer_count, buffer_size, queue_depth)?;
        let mut block = vec![0_u8; buffer_size];
        let copied = block.len().min(self.bulk_data.len());
        block[..copied].copy_from_slice(&self.bulk_data[..copied]);
        Ok(start_stream(
            SyntheticSource::new(vec![block]),
            queue_depth,
            None,
        ))
    }

    fn boxed_clone(&self) -> Box<dyn Transport> {
        Box::new(Self {
            in_replies: self.in_replies.clone(),
            out_replies: self.out_replies.clone(),
            recorded: Vec::new(),
            strict: self.strict,
            bulk_data: self.bulk_data.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_fox_core::{ControlRequest, TransferDirection};

    #[test]
    fn records_control_out_in_arrival_order() {
        let mut mock = MockTransport::new();
        let req = ControlRequest::vendor_out(0, 0x2010, 0x10, vec![0xe8]);
        mock.control_out(&req).unwrap();
        assert_eq!(mock.recorded().len(), 1);
        assert_eq!(
            mock.recorded()[0],
            RecordedRequest {
                direction: TransferDirection::Out,
                request: 0,
                value: 0x2010,
                index: 0x10,
                data: vec![0xe8],
            }
        );
    }

    #[test]
    fn scripted_reply_returned_for_in() {
        let mut mock = MockTransport::new();
        mock.push_reply(ScriptedReply::any_in(vec![0x69]));
        let req = ControlRequest::vendor_in(0, 0x00, 0x06, 1);
        let reply = mock.control_in(&req).unwrap();
        assert_eq!(reply, vec![0x69]);
    }

    #[test]
    fn direction_partition_preserves_fifo_match_precedence() {
        let mut mock = MockTransport::new();
        mock.extend_replies([
            ScriptedReply::any_out(),
            ScriptedReply::any_in(vec![1]),
            ScriptedReply::any_out(),
            ScriptedReply::any_in(vec![2]),
        ]);

        let request = ControlRequest::vendor_in(0, 0, 0, 1);
        assert_eq!(mock.control_in(&request).unwrap(), vec![1]);
        assert_eq!(mock.control_in(&request).unwrap(), vec![2]);
        assert!(mock
            .control_out(&ControlRequest::vendor_out(0, 0, 0, vec![]))
            .is_ok());
        assert!(mock
            .control_out(&ControlRequest::vendor_out(0, 0, 0, vec![]))
            .is_ok());
    }

    #[test]
    fn default_in_returns_zeros_of_requested_length() {
        let mut mock = MockTransport::new();
        let req = ControlRequest::vendor_in(0, 0, 0, 4);
        let reply = mock.control_in(&req).unwrap();
        assert_eq!(reply, vec![0, 0, 0, 0]);
    }

    #[test]
    fn strict_mode_errors_on_unmatched_request() {
        let mut mock = MockTransport::new();
        mock.strict(true);
        let req = ControlRequest::vendor_in(0, 0, 0, 1);
        assert!(mock.control_in(&req).is_err());
    }

    #[test]
    fn selective_reply_matches_on_value_and_index() {
        let mut mock = MockTransport::new();
        mock.push_reply(ScriptedReply {
            direction: TransferDirection::In,
            request: None,
            value: Some(0x1234),
            index: Some(0x56),
            payload: vec![0xaa],
        });
        // Non-matching request returns zeros.
        let r1 = mock
            .control_in(&ControlRequest::vendor_in(0, 0x9999, 0x56, 1))
            .unwrap();
        assert_eq!(r1, vec![0]);
        // Matching request returns the payload.
        let r2 = mock
            .control_in(&ControlRequest::vendor_in(0, 0x1234, 0x56, 1))
            .unwrap();
        assert_eq!(r2, vec![0xaa]);
    }

    #[test]
    fn bulk_read_returns_min_of_len_and_buffered() {
        let mut mock = MockTransport::new();
        mock.set_bulk_data(vec![1, 2, 3, 4, 5]);
        assert_eq!(mock.bulk_read(0x81, 3, 0).unwrap(), vec![1, 2, 3]);
        assert_eq!(mock.bulk_read(0x81, 100, 0).unwrap(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn mock_stream_repeats_padded_bulk_data_and_validates_shape() {
        let mut mock = MockTransport::new();
        mock.set_bulk_data(vec![1, 2, 3, 4]);
        assert!(mock.start_bulk_stream(0x81, 0, 512, 1).is_err());
        let mut stream = mock.start_bulk_stream(0x81, 1, 512, 1).unwrap();
        let block = stream.recv().unwrap().unwrap();
        let sdr_fox_core::IqSamples::Cu8(bytes) = block.samples else {
            panic!("mock transport emits native Cu8");
        };
        assert_eq!(&bytes[..4], &[1, 2, 3, 4]);
        assert!(bytes[4..].iter().all(|&byte| byte == 0));
        stream.stop();
    }

    #[test]
    fn count_matching_finds_specific_requests() {
        let mut mock = MockTransport::new();
        mock.control_out(&ControlRequest::vendor_out(0, 0x3000, 0, vec![]))
            .unwrap();
        mock.control_out(&ControlRequest::vendor_out(0, 0x3000, 0, vec![]))
            .unwrap();
        mock.control_out(&ControlRequest::vendor_out(0, 0x4000, 0, vec![]))
            .unwrap();
        assert_eq!(mock.count_matching(|r| r.value == 0x3000), 2);
    }

    #[test]
    fn boxed_clone_produces_independent_recording_state() {
        // A clone is taken; subsequent requests on the original must still be
        // recorded independently of the clone (which starts with empty state).
        let mut mock = MockTransport::new();
        mock.control_out(&ControlRequest::vendor_out(0, 0, 0, vec![]))
            .unwrap();
        let _clone = mock.boxed_clone();
        // Original keeps its recording.
        assert_eq!(mock.recorded().len(), 1);
        // A new request is appended to the original only.
        mock.control_out(&ControlRequest::vendor_out(0, 1, 0, vec![]))
            .unwrap();
        assert_eq!(mock.recorded().len(), 2);
    }
}
