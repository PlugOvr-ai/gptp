//! Passive packet-drop detection for sensor streams.
//!
//! RTP, IEEE 1722 (AVTP) and SOME/IP all carry per-stream sequence counters,
//! so gaps in the counter measure actual packet loss on the path to our port
//! — no cooperation from the sensors needed. Detection is heuristic (RTP has
//! no magic number), so a stream must first prove itself with a run of
//! strictly consecutive sequence numbers before losses are counted; random
//! payloads that merely look like RTP never confirm.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use crate::state::StreamInfo;

/// Consecutive in-order packets required before a candidate stream is
/// trusted and loss counting starts.
const CONFIRM_RUN: u32 = 16;
/// Idle time after which a stream is dropped.
const STREAM_TIMEOUT_SECS: u64 = 30;
const CANDIDATE_TIMEOUT_SECS: u64 = 5;
const MAX_STREAMS: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Rtp,
    SomeIp,
    Avtp,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Rtp => "rtp",
            Kind::SomeIp => "some/ip",
            Kind::Avtp => "avtp",
        }
    }

    fn seq_bits(self) -> u32 {
        match self {
            Kind::Avtp => 8,
            _ => 16,
        }
    }

    /// Largest forward jump still counted as loss; anything bigger is
    /// treated as reordering/restart and not counted.
    fn max_gap(self) -> u32 {
        match self {
            Kind::Avtp => 32,
            _ => 2048,
        }
    }
}

#[derive(Hash, PartialEq, Eq, Clone)]
enum StreamKey {
    Udp {
        src: [u8; 6],
        dst: Ipv4Addr,
        dst_port: u16,
    },
    Avtp {
        src: [u8; 6],
        stream_id: [u8; 8],
    },
}

struct Stream {
    kind: Kind,
    src: [u8; 6],
    dst_desc: String,
    last_seq: u32,
    confirm_run: u32,
    confirmed: bool,
    packets: u64,
    bytes: u64,
    bytes_last: u64,
    rate_bps: f64,
    lost: u64,
    duplicates: u64,
    out_of_order: u64,
    last_seen: Instant,
    lost_at_last_alarm: u64,
    last_alarm: Option<Instant>,
}

#[derive(Default)]
pub struct StreamTracker {
    streams: HashMap<StreamKey, Stream>,
}

impl StreamTracker {
    /// Feed a UDP packet (already demultiplexed from IPv4).
    pub fn observe_udp(
        &mut self,
        src: [u8; 6],
        dst: Ipv4Addr,
        dst_port: u16,
        payload: &[u8],
        frame_len: usize,
    ) {
        // Housekeeping protocols that would only produce noise.
        if matches!(dst_port, 53 | 67 | 68 | 123 | 5353 | 5355) {
            return;
        }
        let key = StreamKey::Udp {
            src,
            dst,
            dst_port,
        };
        if let Some(stream) = self.streams.get_mut(&key) {
            if let Some(seq) = extract_seq(stream.kind, payload) {
                stream.update(seq, frame_len);
            }
            return;
        }
        // New candidate: RTP first (more specific version check), then SOME/IP.
        for kind in [Kind::Rtp, Kind::SomeIp] {
            if let Some(seq) = extract_seq(kind, payload) {
                self.insert(
                    key,
                    kind,
                    src,
                    format!("{dst}:{dst_port}"),
                    seq,
                    frame_len,
                );
                return;
            }
        }
    }

    /// Feed an IEEE 1722 (EtherType 0x22F0) frame body.
    pub fn observe_avtp(&mut self, src: [u8; 6], body: &[u8], frame_len: usize) {
        // AVTPDU stream common header: subtype, sv|version|…, sequence_num,
        // reserved|tu, stream_id[8]. Only stream data (sv set) has a sequence.
        if body.len() < 12 || body[1] & 0x80 == 0 {
            return;
        }
        let seq = body[2] as u32;
        let mut stream_id = [0u8; 8];
        stream_id.copy_from_slice(&body[4..12]);
        let key = StreamKey::Avtp { src, stream_id };
        if let Some(stream) = self.streams.get_mut(&key) {
            stream.update(seq, frame_len);
        } else {
            let dst_desc = format!(
                "stream {:02x}{:02x}…{:02x}{:02x}",
                stream_id[0], stream_id[1], stream_id[6], stream_id[7]
            );
            self.insert(key, Kind::Avtp, src, dst_desc, seq, frame_len);
        }
    }

    fn insert(
        &mut self,
        key: StreamKey,
        kind: Kind,
        src: [u8; 6],
        dst_desc: String,
        seq: u32,
        frame_len: usize,
    ) {
        if self.streams.len() >= MAX_STREAMS {
            return;
        }
        self.streams.insert(
            key,
            Stream {
                kind,
                src,
                dst_desc,
                last_seq: seq,
                confirm_run: 0,
                confirmed: false,
                packets: 1,
                bytes: frame_len as u64,
                bytes_last: 0,
                rate_bps: 0.0,
                lost: 0,
                duplicates: 0,
                out_of_order: 0,
                last_seen: Instant::now(),
                lost_at_last_alarm: 0,
                last_alarm: None,
            },
        );
    }

    /// Once-per-second housekeeping: rates, pruning; returns alarm messages
    /// for streams whose loss increased (rate-limited per stream).
    pub fn tick(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut alarms = Vec::new();
        self.streams.retain(|_, s| {
            let idle = now.duration_since(s.last_seen).as_secs();
            idle < if s.confirmed {
                STREAM_TIMEOUT_SECS
            } else {
                CANDIDATE_TIMEOUT_SECS
            }
        });
        for s in self.streams.values_mut() {
            s.rate_bps = (s.bytes - s.bytes_last) as f64 * 8.0;
            s.bytes_last = s.bytes;
            let newly_lost = s.lost - s.lost_at_last_alarm;
            if newly_lost > 0
                && s.last_alarm
                    .is_none_or(|t| now.duration_since(t).as_secs() >= 30)
            {
                alarms.push(format!(
                    "ALARM: {} stream {} → {} lost {} packets ({} total)",
                    s.kind.label(),
                    crate::inventory::fmt_mac(&s.src),
                    s.dst_desc,
                    newly_lost,
                    s.lost
                ));
                s.lost_at_last_alarm = s.lost;
                s.last_alarm = Some(now);
            }
        }
        alarms
    }

    /// Confirmed streams for display, busiest first.
    pub fn snapshot(&self) -> Vec<StreamInfo> {
        let mut out: Vec<StreamInfo> = self
            .streams
            .values()
            .filter(|s| s.confirmed)
            .map(|s| StreamInfo {
                kind: s.kind.label(),
                src_mac: s.src,
                dst: s.dst_desc.clone(),
                rate_bps: s.rate_bps,
                packets: s.packets,
                lost: s.lost,
                duplicates: s.duplicates,
                out_of_order: s.out_of_order,
            })
            .collect();
        out.sort_by(|a, b| b.rate_bps.total_cmp(&a.rate_bps));
        out
    }
}

impl Stream {
    fn update(&mut self, seq: u32, frame_len: usize) {
        self.last_seen = Instant::now();
        self.packets += 1;
        self.bytes += frame_len as u64;
        let mask = (1u32 << self.kind.seq_bits()) - 1;
        let delta = seq.wrapping_sub(self.last_seq) & mask;

        if !self.confirmed {
            // Candidates must count strictly consecutively; anything else
            // resets the run. Constant or random fields never confirm.
            if delta == 1 {
                self.confirm_run += 1;
                if self.confirm_run >= CONFIRM_RUN {
                    self.confirmed = true;
                }
            } else {
                self.confirm_run = 0;
            }
            self.last_seq = seq;
            return;
        }

        match delta {
            0 => self.duplicates += 1,
            1 => self.last_seq = seq,
            d if d <= self.kind.max_gap() => {
                self.lost += (d - 1) as u64;
                self.last_seq = seq;
            }
            // Backwards or a huge jump: reordered/late packet or a stream
            // restart — not counted as loss, sequence position kept.
            _ => self.out_of_order += 1,
        }
    }
}

/// Try to read a sequence counter of the given kind from a UDP payload.
fn extract_seq(kind: Kind, p: &[u8]) -> Option<u32> {
    match kind {
        Kind::Rtp => {
            // RTP: version 2 in the top bits, sequence number at offset 2.
            if p.len() >= 12 && p[0] >> 6 == 2 {
                Some(u16::from_be_bytes([p[2], p[3]]) as u32)
            } else {
                None
            }
        }
        Kind::SomeIp => {
            // SOME/IP: protocol version 1 at offset 12, known message type
            // at 14; the session id (offset 10) increments per sender.
            if p.len() >= 16
                && p[12] == 0x01
                && matches!(p[14], 0x00 | 0x01 | 0x02 | 0x40 | 0x41 | 0x80 | 0x81 | 0xA0 | 0xA1)
            {
                Some(u16::from_be_bytes([p[10], p[11]]) as u32)
            } else {
                None
            }
        }
        Kind::Avtp => None, // handled by observe_avtp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const DST: Ipv4Addr = Ipv4Addr::new(239, 0, 0, 5);

    fn rtp_packet(seq: u16) -> Vec<u8> {
        let mut p = vec![0u8; 12];
        p[0] = 0x80; // version 2
        p[2..4].copy_from_slice(&seq.to_be_bytes());
        p
    }

    fn feed(t: &mut StreamTracker, seq: u16) {
        t.observe_udp(SRC, DST, 5004, &rtp_packet(seq), 1000);
    }

    fn stats(t: &StreamTracker) -> StreamInfo {
        t.snapshot().into_iter().next().expect("confirmed stream")
    }

    #[test]
    fn rtp_gap_counting_with_wraparound() {
        let mut t = StreamTracker::default();
        // Confirmation run crossing the 16-bit wrap.
        for i in 0..20u32 {
            feed(&mut t, (65530u32 + i) as u16);
        }
        assert_eq!(stats(&t).lost, 0);
        let next = (65530u32 + 20) as u16;
        // Drop 3 packets.
        feed(&mut t, next.wrapping_add(3));
        assert_eq!(stats(&t).lost, 3);
        // Duplicate.
        feed(&mut t, next.wrapping_add(3));
        assert_eq!(stats(&t).duplicates, 1);
        // Late/reordered packet: not loss.
        feed(&mut t, next.wrapping_sub(5));
        let s = stats(&t);
        assert_eq!(s.out_of_order, 1);
        assert_eq!(s.lost, 3);
    }

    #[test]
    fn constant_counter_never_confirms() {
        let mut t = StreamTracker::default();
        // Looks RTP-ish, but the "sequence" field never changes (e.g. a
        // random payload that happens to have the version bits set).
        for _ in 0..100 {
            feed(&mut t, 42);
        }
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn someip_session_gap() {
        let mut t = StreamTracker::default();
        let pkt = |session: u16| {
            let mut p = vec![0u8; 16];
            p[10..12].copy_from_slice(&session.to_be_bytes());
            p[12] = 0x01; // protocol version
            p[14] = 0x02; // notification
            p
        };
        for i in 1..=20u16 {
            t.observe_udp(SRC, DST, 30490, &pkt(i), 200);
        }
        t.observe_udp(SRC, DST, 30490, &pkt(25), 200);
        let s = stats(&t);
        assert_eq!(s.kind, "some/ip");
        assert_eq!(s.lost, 4);
    }

    #[test]
    fn avtp_8bit_sequence() {
        let mut t = StreamTracker::default();
        let frame = |seq: u8| {
            let mut b = vec![0u8; 24];
            b[0] = 0x02; // AAF
            b[1] = 0x80; // sv
            b[2] = seq;
            b[4..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            b
        };
        // Confirmation run crossing the 8-bit wrap.
        for i in 0..20u32 {
            t.observe_avtp(SRC, &frame((250u32 + i) as u8), 100);
        }
        t.observe_avtp(SRC, &frame((250u32 + 22) as u8), 100);
        let s = stats(&t);
        assert_eq!(s.kind, "avtp");
        assert_eq!(s.lost, 2);
    }

    #[test]
    fn loss_alarms_are_rate_limited() {
        let mut t = StreamTracker::default();
        for i in 0..20u16 {
            feed(&mut t, i);
        }
        feed(&mut t, 25); // 5 lost
        let alarms = t.tick();
        assert_eq!(alarms.len(), 1);
        assert!(alarms[0].contains("lost 5 packets"));
        feed(&mut t, 28); // 2 more lost, but within the rate limit
        assert!(t.tick().is_empty());
    }
}
