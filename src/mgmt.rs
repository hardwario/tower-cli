//! Host-side management-channel session helpers (wire v3): issue one `MgmtRequest`
//! over a [`Transport`] and reassemble its chunked `MgmtResponse`. Same
//! transport-generic design as `session.rs` (testable over `MockPort`); shared by the
//! `tower gateway` startup verification, the gateway engine's synchronous paths, and
//! the cable-pairing client (`nodes add --port`, which drives a *node's* port).
//!
//! Also home of the **owned** mirrors of the borrowed `tower_protocol::mgmt` reply
//! records (the wire types borrow the frame buffer; everything above the transport
//! wants owned data).

use std::time::{Duration, Instant};

use tower_protocol::mgmt::{DeviceInfo, DeviceRole, MgmtOp, NodeEntry, QueueEntry};
use tower_protocol::msg::{MgmtRequest, MgmtResponse};
use tower_protocol::{Error, FrameDecoder, MAX_WIRE, MsgType, decode_frame, encode_frame};

use crate::session::Transport;

/// Owned `DeviceInfo` — what `Describe` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceInfoOwned {
    pub role: DeviceRole,
    pub radio_schema_version: u8,
    pub net_id: u32,
    pub band: u8,
    pub channel: u8,
    pub node_capacity: u8,
    pub node_count: u8,
    pub provisioned: bool,
    pub gw_id: u32,
    pub firmware_name: String,
}

impl From<DeviceInfo<'_>> for DeviceInfoOwned {
    fn from(d: DeviceInfo<'_>) -> Self {
        Self {
            role: d.role,
            radio_schema_version: d.radio_schema_version,
            net_id: d.net_id,
            band: d.band,
            channel: d.channel,
            node_capacity: d.node_capacity,
            node_count: d.node_count,
            provisioned: d.provisioned,
            gw_id: d.gw_id,
            firmware_name: d.firmware_name.to_string(),
        }
    }
}

/// Owned `NodeEntry` — one registry row from `NodeList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeEntryOwned {
    pub id: u32,
    pub name: String,
    pub flags: u8,
    pub last_seen_s: u32,
    pub rssi_dbm: i8,
    pub uplinks: u32,
    pub queued: u8,
}

impl From<NodeEntry<'_>> for NodeEntryOwned {
    fn from(e: NodeEntry<'_>) -> Self {
        Self {
            id: e.id,
            name: e.name.to_string(),
            flags: e.flags,
            last_seen_s: e.last_seen_s,
            rssi_dbm: e.rssi_dbm,
            uplinks: e.uplinks,
            queued: e.queued,
        }
    }
}

/// Owned `QueueEntry` — one pending downlink from `QueueList`. (Engine paths decode
/// the borrowed records directly today; this owned mirror serves the HIL-style
/// consumers and keeps the record set symmetric.)
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueEntryOwned {
    pub node: u32,
    pub item: u16,
    pub age_s: u16,
    pub ttl_s: u16,
    pub data: Vec<u8>,
}

impl From<QueueEntry<'_>> for QueueEntryOwned {
    fn from(e: QueueEntry<'_>) -> Self {
        Self {
            node: e.node,
            item: e.item,
            age_s: e.age_s,
            ttl_s: e.ttl_s,
            data: e.data.to_vec(),
        }
    }
}

/// Decode a reassembled record stream with repeated `take_from_bytes` (the chunked
/// `MgmtResponse.data` contract). Stops at the first undecodable tail.
pub(crate) fn parse_records<'a, T: serde::Deserialize<'a>>(mut data: &'a [u8]) -> Vec<T> {
    let mut out = Vec::new();
    while !data.is_empty() {
        match postcard::take_from_bytes::<T>(data) {
            Ok((rec, rest)) => {
                out.push(rec);
                data = rest;
            }
            Err(_) => break,
        }
    }
    out
}

/// A complete management reply: the final `result` code plus the concatenated record
/// stream (`data`), with the chunk-gap truncation flag (mirror of `session::Response`).
#[derive(Debug)]
pub(crate) struct MgmtReply {
    pub result: u8,
    pub data: Vec<u8>,
    /// A chunk gap was seen; `data` is silently missing a middle piece.
    #[allow(dead_code)] // read by tests + future strict callers; parity with session::Response
    pub truncated: bool,
}

/// The outcome of waiting for a management reply — tri-state like `session::ReadOutcome`
/// so a lockstep mismatch is diagnosable (exit 125) instead of reading as a mute device.
pub(crate) enum MgmtOutcome {
    Reply(MgmtReply),
    Timeout { bad_version: Option<u8> },
}

/// Encode one `MgmtRequest` frame (requests always fit a single frame).
pub(crate) fn encode_mgmt(seq: u16, req_id: u16, op: &MgmtOp<'_>) -> Option<Vec<u8>> {
    let mut buf = [0u8; MAX_WIRE];
    let n = encode_frame(
        MsgType::MgmtRequest,
        seq,
        &MgmtRequest {
            req_id,
            op: op.clone(),
        },
        &mut buf,
    )
    .ok()?;
    Some(buf[..n].to_vec())
}

/// Send `op` and reassemble its reply. `timeout` is an *idle* deadline (reset per
/// matching chunk), like `session::read_response`. Non-matching frames (logs, uplinks,
/// other req_ids) are ignored. NOT for delayed ops (`PairingOpen`/`JoinOpen` can answer
/// up to their window later — the engine's event loop owns those).
pub(crate) fn mgmt_roundtrip(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    req_id: u16,
    op: &MgmtOp<'_>,
    timeout: Duration,
) -> MgmtOutcome {
    let mut bad_version: Option<u8> = None;
    let Some(frame) = encode_mgmt(0, req_id, op) else {
        return MgmtOutcome::Timeout { bad_version };
    };
    if sp.write_all(&frame).is_err() || sp.flush().is_err() {
        return MgmtOutcome::Timeout { bad_version };
    }

    let mut deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut next_chunk: u16 = 0;
    let mut truncated = false;
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        let nread = match sp.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(_) => return MgmtOutcome::Timeout { bad_version },
        };
        for &b in &buf[..nread] {
            let Some(inner) = dec.push(b) else { continue };
            let (mt, _, payload) = match decode_frame(inner) {
                Ok(t) => t,
                Err(Error::BadVersion { got }) => {
                    bad_version = Some(got);
                    continue;
                }
                Err(_) => continue,
            };
            if mt != MsgType::MgmtResponse {
                continue;
            }
            let Ok(r) = postcard::from_bytes::<MgmtResponse>(payload) else {
                continue;
            };
            if r.req_id != req_id {
                continue;
            }
            deadline = Instant::now() + timeout;
            if r.chunk != next_chunk {
                truncated = true;
            }
            next_chunk = r.chunk.wrapping_add(1);
            data.extend_from_slice(r.data);
            if r.last {
                return MgmtOutcome::Reply(MgmtReply {
                    result: r.result,
                    data,
                    truncated,
                });
            }
        }
    }
    MgmtOutcome::Timeout { bad_version }
}

/// The role probe: `Describe`, decoded. This is the authoritative "is this a
/// gateway / node?" check — stronger than `Hello.firmware_name`, which is display-only
/// (pre-v3 firmware never answers; a wrong role answers honestly).
pub(crate) enum DescribeOutcome {
    Info(DeviceInfoOwned),
    /// The device answered but refused (result code) — malformed/unsupported.
    Refused(u8),
    Timeout {
        bad_version: Option<u8>,
    },
}

pub(crate) fn describe(
    sp: &mut (impl Transport + ?Sized),
    dec: &mut FrameDecoder,
    req_id: u16,
    timeout: Duration,
) -> DescribeOutcome {
    match mgmt_roundtrip(sp, dec, req_id, &MgmtOp::Describe, timeout) {
        MgmtOutcome::Reply(r) if r.result == tower_protocol::mgmt::MGMT_OK => {
            match postcard::from_bytes::<DeviceInfo>(&r.data) {
                Ok(info) => DescribeOutcome::Info(info.into()),
                Err(_) => DescribeOutcome::Refused(u8::MAX),
            }
        }
        MgmtOutcome::Reply(r) => DescribeOutcome::Refused(r.result),
        MgmtOutcome::Timeout { bad_version } => DescribeOutcome::Timeout { bad_version },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use tower_protocol::mgmt::{MGMT_OK, NodeEntry};

    /// In-memory duplex transport (same contract as main.rs's MockPort: drained reads
    /// return TimedOut like a real serial port).
    struct Mock {
        to_read: std::collections::VecDeque<u8>,
        written: Vec<u8>,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                to_read: Default::default(),
                written: Vec::new(),
            }
        }
        fn feed(&mut self, bytes: &[u8]) {
            self.to_read.extend(bytes.iter().copied());
        }
    }

    impl Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.to_read.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "drained"));
            }
            let n = buf.len().min(self.to_read.len());
            for b in buf.iter_mut().take(n) {
                *b = self.to_read.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl std::io::Write for Mock {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mgmt_response_frame(
        req_id: u16,
        result: u8,
        chunk: u16,
        last: bool,
        data: &[u8],
    ) -> Vec<u8> {
        let mut buf = [0u8; MAX_WIRE];
        let n = encode_frame(
            MsgType::MgmtResponse,
            chunk,
            &MgmtResponse {
                req_id,
                result,
                chunk,
                last,
                data,
            },
            &mut buf,
        )
        .unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn roundtrip_reassembles_chunks() {
        let mut m = Mock::new();
        m.feed(&mgmt_response_frame(7, 0, 0, false, b"abc"));
        m.feed(&mgmt_response_frame(7, MGMT_OK, 1, true, b"def"));
        // A foreign req_id in between is ignored.
        m.feed(&mgmt_response_frame(9, 1, 0, true, b"zzz"));
        let mut dec = FrameDecoder::new();
        match mgmt_roundtrip(
            &mut m,
            &mut dec,
            7,
            &MgmtOp::NodeList,
            Duration::from_millis(300),
        ) {
            MgmtOutcome::Reply(r) => {
                assert_eq!(r.result, MGMT_OK);
                assert_eq!(r.data, b"abcdef");
                assert!(!r.truncated);
            }
            _ => panic!("expected a reply"),
        }
        // The request went out as a MgmtRequest frame.
        let mut dec2 = FrameDecoder::new();
        let inner: Vec<u8> = m
            .written
            .iter()
            .find_map(|&b| dec2.push(b).map(|s| s.to_vec()))
            .expect("one request frame");
        let (mt, _, _) = decode_frame(&inner).unwrap();
        assert_eq!(mt, MsgType::MgmtRequest);
    }

    #[test]
    fn chunk_gap_flags_truncated() {
        let mut m = Mock::new();
        m.feed(&mgmt_response_frame(3, 0, 0, false, b"a"));
        m.feed(&mgmt_response_frame(3, MGMT_OK, 2, true, b"c")); // chunk 1 lost
        let mut dec = FrameDecoder::new();
        match mgmt_roundtrip(
            &mut m,
            &mut dec,
            3,
            &MgmtOp::NodeList,
            Duration::from_millis(300),
        ) {
            MgmtOutcome::Reply(r) => assert!(r.truncated),
            _ => panic!("expected a reply"),
        }
    }

    #[test]
    fn timeout_when_mute() {
        let mut m = Mock::new();
        let mut dec = FrameDecoder::new();
        match mgmt_roundtrip(
            &mut m,
            &mut dec,
            1,
            &MgmtOp::Describe,
            Duration::from_millis(50),
        ) {
            MgmtOutcome::Timeout { bad_version: None } => {}
            _ => panic!("expected a clean timeout"),
        }
    }

    #[test]
    fn describe_decodes_device_info() {
        let info = DeviceInfo {
            role: DeviceRole::Gateway,
            radio_schema_version: 1,
            net_id: 0xAB12,
            band: 0,
            channel: 0,
            node_capacity: 32,
            node_count: 2,
            provisioned: true,
            gw_id: 0xAB12,
            firmware_name: "radio_dongle_gateway",
        };
        let rec = postcard::to_stdvec(&info).unwrap();
        let mut m = Mock::new();
        m.feed(&mgmt_response_frame(1, MGMT_OK, 0, true, &rec));
        let mut dec = FrameDecoder::new();
        match describe(&mut m, &mut dec, 1, Duration::from_millis(300)) {
            DescribeOutcome::Info(d) => {
                assert_eq!(d.role, DeviceRole::Gateway);
                assert_eq!(d.net_id, 0xAB12);
                assert_eq!(d.firmware_name, "radio_dongle_gateway");
            }
            _ => panic!("expected device info"),
        }
    }

    #[test]
    fn record_stream_parses() {
        let a = NodeEntry {
            id: 1,
            name: "a",
            flags: 0,
            last_seen_s: 5,
            rssi_dbm: -60,
            uplinks: 2,
            queued: 0,
        };
        let b = NodeEntry {
            id: 2,
            name: "b",
            flags: 1,
            last_seen_s: u32::MAX,
            rssi_dbm: i8::MAX,
            uplinks: 0,
            queued: 1,
        };
        let mut stream = postcard::to_stdvec(&a).unwrap();
        stream.extend(postcard::to_stdvec(&b).unwrap());
        let parsed: Vec<NodeEntry> = parse_records(&stream);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, 1);
        assert_eq!(parsed[1].name, "b");
        let owned: Vec<NodeEntryOwned> = parsed.into_iter().map(Into::into).collect();
        assert_eq!(owned[1].flags, 1);
    }
}
