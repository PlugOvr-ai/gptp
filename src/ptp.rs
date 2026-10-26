//! IEEE 802.1AS (gPTP) message building and parsing, and the IEEE 1588
//! default-profile messages served beside it (`--ptp`).
//!
//! gPTP runs directly over Ethernet (EtherType 0x88F7) to the link-local
//! multicast address 01:80:C2:00:00:0E, with majorSdoId (transportSpecific)
//! 1. 1588 uses majorSdoId 0, over UDP/IPv4 or Ethernet (01:1B:19:00:00:00).

pub const ETH_P_1588: u16 = 0x88F7;
pub const PTP_MCAST: [u8; 6] = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E];
/// IEEE 1588 over Ethernet, all messages except peer delay.
pub const PTP1588_MCAST: [u8; 6] = [0x01, 0x1B, 0x19, 0x00, 0x00, 0x00];

/// majorSdoId (transportSpecific) in the first header byte.
pub const SDO_GPTP: u8 = 0x10;
pub const SDO_1588: u8 = 0x00;

pub const MT_SYNC: u8 = 0x0;
pub const MT_DELAY_REQ: u8 = 0x1;
pub const MT_PDELAY_REQ: u8 = 0x2;
pub const MT_PDELAY_RESP: u8 = 0x3;
pub const MT_FOLLOW_UP: u8 = 0x8;
pub const MT_DELAY_RESP: u8 = 0x9;
pub const MT_PDELAY_RESP_FUP: u8 = 0xA;
pub const MT_ANNOUNCE: u8 = 0xB;

const FLAG_TWO_STEP: u16 = 0x0200;
const FLAG_PTP_TIMESCALE: u16 = 0x0008;
const FLAG_UTC_OFFSET_VALID: u16 = 0x0004;

const HDR_LEN: usize = 34;

/// PTP timestamp: 48-bit seconds + 32-bit nanoseconds (TAI timescale).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn to_ns(self) -> i128 {
        self.secs as i128 * 1_000_000_000 + self.nanos as i128
    }

    pub fn from_ns(ns: i128) -> Self {
        let ns = ns.max(0);
        Timestamp {
            secs: (ns / 1_000_000_000) as u64,
            nanos: (ns % 1_000_000_000) as u32,
        }
    }

    fn write(&self, buf: &mut [u8]) {
        buf[0..6].copy_from_slice(&self.secs.to_be_bytes()[2..8]);
        buf[6..10].copy_from_slice(&self.nanos.to_be_bytes());
    }

    fn parse(b: &[u8]) -> Self {
        let mut s = [0u8; 8];
        s[2..8].copy_from_slice(&b[0..6]);
        Timestamp {
            secs: u64::from_be_bytes(s),
            nanos: u32::from_be_bytes([b[6], b[7], b[8], b[9]]),
        }
    }
}

/// Grandmaster fields carried in an Announce message, used for BMCA comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceData {
    pub priority1: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub variance: u16,
    pub priority2: u8,
    pub gm_identity: [u8; 8],
    pub steps_removed: u16,
    pub time_source: u8,
}

impl AnnounceData {
    /// Dataset comparison key: lower compares as better, per IEEE 1588 BMCA order.
    pub fn bmca_key(&self) -> (u8, u8, u8, u16, u8, [u8; 8]) {
        (
            self.priority1,
            self.clock_class,
            self.clock_accuracy,
            self.variance,
            self.priority2,
            self.gm_identity,
        )
    }
}

/// 802.1AS Follow_Up information TLV contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FollowUpInfo {
    /// (rateRatio - 1) * 2^41: GM frequency relative to the local relay.
    pub cumulative_scaled_rate_offset: i32,
    /// Changes whenever the GM's time base changes (new GM, GM phase/freq step).
    pub gm_time_base_indicator: u16,
    /// Phase change of the GM at its last time base change, ns.
    pub last_gm_phase_change_ns: i64,
    pub scaled_last_gm_freq_change: i32,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Announce {
        source: [u8; 10],
        log_interval: i8,
        data: AnnounceData,
        /// currentUtcOffset (TAI − UTC, s) and whether the master marks it
        /// valid (currentUtcOffsetValid flag).
        utc_offset: i16,
        utc_offset_valid: bool,
    },
    Sync {
        source: [u8; 10],
        seq: u16,
        log_interval: i8,
        two_step: bool,
        /// originTimestamp; only meaningful for one-step Sync.
        origin: Timestamp,
        correction_ns: i64,
    },
    FollowUp {
        source: [u8; 10],
        seq: u16,
        precise_origin: Timestamp,
        correction_ns: i64,
        info: Option<FollowUpInfo>,
    },
    PdelayReq {
        seq: u16,
        source: [u8; 10],
    },
    /// 1588 end-to-end delay request (to a master).
    DelayReq {
        seq: u16,
        source: [u8; 10],
        correction_ns: i64,
    },
    /// 1588 answer to a Delay_Req: the master's receive time of it.
    DelayResp {
        source: [u8; 10],
        seq: u16,
        log_interval: i8,
        receive: Timestamp,
        requesting: [u8; 10],
        correction_ns: i64,
    },
    PdelayResp {
        source: [u8; 10],
        seq: u16,
        req_receipt: Timestamp,
        requesting: [u8; 10],
        correction_ns: i64,
    },
    PdelayRespFup {
        source: [u8; 10],
        seq: u16,
        resp_origin: Timestamp,
        requesting: [u8; 10],
        correction_ns: i64,
    },
}

/// Parse a PTP payload (Ethernet header already stripped).
pub fn parse(buf: &[u8]) -> Option<Msg> {
    if buf.len() < HDR_LEN {
        return None;
    }
    let msg_type = buf[0] & 0x0F;
    let length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if buf.len() < length {
        return None;
    }
    let mut source = [0u8; 10];
    source.copy_from_slice(&buf[20..30]);
    let seq = u16::from_be_bytes([buf[30], buf[31]]);
    let flags = u16::from_be_bytes([buf[6], buf[7]]);
    let log_interval = buf[33] as i8;
    // correctionField is a signed 2^16-scaled nanosecond value.
    let correction_ns = (i64::from_be_bytes(buf[8..16].try_into().ok()?)) >> 16;

    match msg_type {
        MT_ANNOUNCE => {
            if length < HDR_LEN + 30 {
                return None;
            }
            let mut gm_identity = [0u8; 8];
            gm_identity.copy_from_slice(&buf[53..61]);
            Some(Msg::Announce {
                source,
                log_interval,
                utc_offset: i16::from_be_bytes([buf[44], buf[45]]),
                utc_offset_valid: flags & FLAG_UTC_OFFSET_VALID != 0,
                data: AnnounceData {
                    priority1: buf[47],
                    clock_class: buf[48],
                    clock_accuracy: buf[49],
                    variance: u16::from_be_bytes([buf[50], buf[51]]),
                    priority2: buf[52],
                    gm_identity,
                    steps_removed: u16::from_be_bytes([buf[61], buf[62]]),
                    time_source: buf[63],
                },
            })
        }
        MT_SYNC => {
            if length < HDR_LEN + 10 {
                return None;
            }
            Some(Msg::Sync {
                source,
                seq,
                log_interval,
                two_step: flags & FLAG_TWO_STEP != 0,
                origin: Timestamp::parse(&buf[34..44]),
                correction_ns,
            })
        }
        MT_FOLLOW_UP => {
            if length < HDR_LEN + 10 {
                return None;
            }
            let info = (length >= HDR_LEN + 10 + 32
                && buf[44..46] == [0x00, 0x03]
                && buf[48..54] == [0x00, 0x80, 0xC2, 0x00, 0x00, 0x01])
            .then(|| {
                // lastGmPhaseChange is a 96-bit ScaledNs (ns * 2^16).
                let mut phase = [0u8; 16];
                phase[4..16].copy_from_slice(&buf[60..72]);
                let phase = (i128::from_be_bytes(phase) << 32 >> 32) >> 16;
                FollowUpInfo {
                    cumulative_scaled_rate_offset: i32::from_be_bytes(
                        buf[54..58].try_into().unwrap(),
                    ),
                    gm_time_base_indicator: u16::from_be_bytes([buf[58], buf[59]]),
                    last_gm_phase_change_ns: phase as i64,
                    scaled_last_gm_freq_change: i32::from_be_bytes(buf[72..76].try_into().unwrap()),
                }
            });
            Some(Msg::FollowUp {
                source,
                seq,
                precise_origin: Timestamp::parse(&buf[34..44]),
                correction_ns,
                info,
            })
        }
        MT_PDELAY_REQ => Some(Msg::PdelayReq { seq, source }),
        MT_DELAY_REQ => Some(Msg::DelayReq {
            seq,
            source,
            correction_ns,
        }),
        MT_DELAY_RESP => {
            if length < HDR_LEN + 20 {
                return None;
            }
            let mut requesting = [0u8; 10];
            requesting.copy_from_slice(&buf[44..54]);
            Some(Msg::DelayResp {
                source,
                seq,
                log_interval,
                receive: Timestamp::parse(&buf[34..44]),
                requesting,
                correction_ns,
            })
        }
        MT_PDELAY_RESP => {
            if length < HDR_LEN + 20 {
                return None;
            }
            let mut requesting = [0u8; 10];
            requesting.copy_from_slice(&buf[44..54]);
            Some(Msg::PdelayResp {
                source,
                seq,
                req_receipt: Timestamp::parse(&buf[34..44]),
                requesting,
                correction_ns,
            })
        }
        MT_PDELAY_RESP_FUP => {
            if length < HDR_LEN + 20 {
                return None;
            }
            let mut requesting = [0u8; 10];
            requesting.copy_from_slice(&buf[44..54]);
            Some(Msg::PdelayRespFup {
                source,
                seq,
                resp_origin: Timestamp::parse(&buf[34..44]),
                requesting,
                correction_ns,
            })
        }
        _ => None,
    }
}

/// majorSdoId of a PTP payload: SDO_GPTP or SDO_1588 (or other profiles).
pub fn sdo(buf: &[u8]) -> Option<u8> {
    buf.first().map(|b| b & 0xF0)
}

/// Builds outgoing PTP messages for one port.
pub struct MsgBuilder {
    pub clock_identity: [u8; 8],
    pub port_number: u16,
    pub domain: u8,
    /// SDO_GPTP or SDO_1588.
    pub sdo: u8,
}

impl MsgBuilder {
    fn header(
        &self,
        msg_type: u8,
        length: usize,
        flags: u16,
        seq: u16,
        control: u8,
        log_interval: i8,
    ) -> Vec<u8> {
        let mut b = vec![0u8; length];
        b[0] = self.sdo | msg_type; // majorSdoId | messageType
        b[1] = 0x02; // versionPTP 2
        b[2..4].copy_from_slice(&(length as u16).to_be_bytes());
        b[4] = self.domain;
        b[6..8].copy_from_slice(&flags.to_be_bytes());
        // correctionField (8..16) = 0, messageTypeSpecific (16..20) = 0
        b[20..28].copy_from_slice(&self.clock_identity);
        b[28..30].copy_from_slice(&self.port_number.to_be_bytes());
        b[30..32].copy_from_slice(&seq.to_be_bytes());
        b[32] = control;
        b[33] = log_interval as u8;
        b
    }

    #[allow(clippy::too_many_arguments)]
    pub fn announce(
        &self,
        seq: u16,
        log_announce: i8,
        utc_offset: i16,
        priority1: u8,
        clock_class: u8,
        clock_accuracy: u8,
        variance: u16,
        priority2: u8,
        time_source: u8,
    ) -> Vec<u8> {
        // header + announce body (30) + path trace TLV (12)
        let mut b = self.header(
            MT_ANNOUNCE,
            HDR_LEN + 30 + 12,
            // The UTC offset comes from the kernel or the configured value
            // (37 s since 2017); slaves use it to derive UTC from PTP time.
            FLAG_PTP_TIMESCALE | FLAG_UTC_OFFSET_VALID,
            seq,
            5,
            log_announce,
        );
        // originTimestamp (34..44) stays zero per 802.1AS
        b[44..46].copy_from_slice(&utc_offset.to_be_bytes());
        b[47] = priority1;
        b[48] = clock_class;
        b[49] = clock_accuracy;
        b[50..52].copy_from_slice(&variance.to_be_bytes());
        b[52] = priority2;
        b[53..61].copy_from_slice(&self.clock_identity);
        b[61..63].copy_from_slice(&0u16.to_be_bytes()); // stepsRemoved
        b[63] = time_source;
        // Path trace TLV (required by 802.1AS): our clockIdentity is the whole path.
        b[64..66].copy_from_slice(&0x0008u16.to_be_bytes());
        b[66..68].copy_from_slice(&8u16.to_be_bytes());
        b[68..76].copy_from_slice(&self.clock_identity);
        b
    }

    pub fn sync(&self, seq: u16, log_sync: i8) -> Vec<u8> {
        // Two-step: originTimestamp stays zero, precise value goes in Follow_Up.
        self.header(MT_SYNC, HDR_LEN + 10, FLAG_TWO_STEP, seq, 0, log_sync)
    }

    pub fn follow_up(&self, seq: u16, origin: Timestamp, log_sync: i8) -> Vec<u8> {
        // header + preciseOriginTimestamp (10) + 802.1AS Follow_Up information TLV (32)
        let mut b = self.header(MT_FOLLOW_UP, HDR_LEN + 10 + 32, 0, seq, 2, log_sync);
        origin.write(&mut b[34..44]);
        b[44..46].copy_from_slice(&0x0003u16.to_be_bytes()); // ORGANIZATION_EXTENSION
        b[46..48].copy_from_slice(&28u16.to_be_bytes());
        b[48..51].copy_from_slice(&[0x00, 0x80, 0xC2]); // IEEE 802.1 OUI
        b[51..54].copy_from_slice(&[0x00, 0x00, 0x01]);
        // cumulativeScaledRateOffset, gmTimeBaseIndicator, lastGmPhaseChange,
        // scaledLastGmFreqChange all zero: we are the time base and never changed.
        b
    }

    /// 1588 Follow_Up: no gPTP information TLV.
    pub fn follow_up_1588(&self, seq: u16, origin: Timestamp, log_sync: i8) -> Vec<u8> {
        let mut b = self.header(MT_FOLLOW_UP, HDR_LEN + 10, 0, seq, 2, log_sync);
        origin.write(&mut b[34..44]);
        b
    }

    /// 1588 Delay_Resp: when the Delay_Req arrived (t4), and the request's
    /// correctionField handed back.
    pub fn delay_resp(
        &self,
        req_seq: u16,
        requesting: &[u8; 10],
        t4: Timestamp,
        req_correction_ns: i64,
        log_min_delay_req: i8,
    ) -> Vec<u8> {
        let mut b = self.header(
            MT_DELAY_RESP,
            HDR_LEN + 20,
            0,
            req_seq,
            3,
            log_min_delay_req,
        );
        b[8..16].copy_from_slice(&(req_correction_ns << 16).to_be_bytes());
        t4.write(&mut b[34..44]);
        b[44..54].copy_from_slice(requesting);
        b
    }

    /// 1588 Delay_Req (two-step: originTimestamp left zero).
    pub fn delay_req(&self, seq: u16) -> Vec<u8> {
        self.header(MT_DELAY_REQ, HDR_LEN + 10, 0, seq, 1, 0x7F)
    }

    pub fn pdelay_req(&self, seq: u16) -> Vec<u8> {
        self.header(MT_PDELAY_REQ, HDR_LEN + 20, 0, seq, 5, 0)
    }

    pub fn pdelay_resp(&self, req_seq: u16, requesting: &[u8; 10], t2: Timestamp) -> Vec<u8> {
        let mut b = self.header(
            MT_PDELAY_RESP,
            HDR_LEN + 20,
            FLAG_TWO_STEP,
            req_seq,
            5,
            0x7F,
        );
        t2.write(&mut b[34..44]);
        b[44..54].copy_from_slice(requesting);
        b
    }

    pub fn pdelay_resp_fup(&self, req_seq: u16, requesting: &[u8; 10], t3: Timestamp) -> Vec<u8> {
        let mut b = self.header(MT_PDELAY_RESP_FUP, HDR_LEN + 20, 0, req_seq, 5, 0x7F);
        t3.write(&mut b[34..44]);
        b[44..54].copy_from_slice(requesting);
        b
    }
}

/// Derive an EUI-64 clock identity from the interface MAC address.
pub fn clock_identity_from_mac(mac: [u8; 6]) -> [u8; 8] {
    [mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]]
}

pub fn fmt_clock_identity(id: &[u8; 8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}.{:02x}{:02x}.{:02x}{:02x}{:02x}",
        id[0], id[1], id[2], id[3], id[4], id[5], id[6], id[7]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> MsgBuilder {
        MsgBuilder {
            clock_identity: [0xAA, 0xBB, 0xCC, 0xFF, 0xFE, 0x01, 0x02, 0x03],
            port_number: 1,
            domain: 0,
            sdo: SDO_GPTP,
        }
    }

    #[test]
    fn ptp1588_messages() {
        let b = MsgBuilder {
            sdo: SDO_1588,
            port_number: 2,
            domain: 4,
            ..builder()
        };
        let sync = b.sync(5, 0);
        assert_eq!(sync[0], 0x00);
        assert_eq!(sync[4], 4);
        assert_eq!(sdo(&sync), Some(SDO_1588));
        let fup = b.follow_up_1588(5, Timestamp { secs: 7, nanos: 8 }, 0);
        assert_eq!(fup.len(), 44);
        assert_eq!((fup[0], fup[32]), (0x08, 2));
        match parse(&fup) {
            Some(Msg::FollowUp {
                precise_origin,
                info: None,
                ..
            }) => assert_eq!(precise_origin, Timestamp { secs: 7, nanos: 8 }),
            other => panic!("unexpected: {other:?}"),
        }

        // A slave's Delay_Req, answered with its sequence, port and correction.
        let mut req = MsgBuilder {
            sdo: SDO_1588,
            clock_identity: [9; 8],
            port_number: 1,
            domain: 4,
        }
        .delay_req(77);
        assert_eq!((req.len(), req[0], req[32], req[33]), (44, 0x01, 1, 0x7F));
        req[8..16].copy_from_slice(&(12i64 << 16).to_be_bytes());
        let Some(Msg::DelayReq {
            seq,
            source,
            correction_ns,
        }) = parse(&req)
        else {
            panic!("Delay_Req not parsed")
        };
        assert_eq!((seq, correction_ns), (77, 12));
        let t4 = Timestamp {
            secs: 100,
            nanos: 5,
        };
        let resp = b.delay_resp(seq, &source, t4, correction_ns, 0);
        assert_eq!(resp.len(), 54);
        assert_eq!((resp[0], resp[32], resp[33]), (0x09, 3, 0));
        assert_eq!(&resp[30..32], &77u16.to_be_bytes());
        assert_eq!(&resp[8..16], &(12i64 << 16).to_be_bytes());
        assert_eq!(Timestamp::parse(&resp[34..44]), t4);
        assert_eq!(&resp[44..54], &source);
        match parse(&resp) {
            Some(Msg::DelayResp {
                seq: 77,
                receive,
                requesting,
                correction_ns: 12,
                ..
            }) => assert_eq!((receive, requesting), (t4, source)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn timestamp_roundtrip() {
        let ts = Timestamp {
            secs: 0x0000_1234_5678_9ABC,
            nanos: 999_999_999,
        };
        let mut buf = [0u8; 10];
        ts.write(&mut buf);
        assert_eq!(Timestamp::parse(&buf), ts);
    }

    #[test]
    fn announce_roundtrip() {
        let b = builder().announce(7, 0, 37, 1, 248, 0xFE, 0x4E5D, 1, 0xA0);
        assert_eq!(b.len(), 76);
        assert_eq!(b[0], 0x1B); // majorSdoId 1, Announce
        match parse(&b) {
            Some(Msg::Announce {
                data,
                utc_offset,
                utc_offset_valid,
                ..
            }) => {
                assert_eq!(utc_offset, 37);
                assert!(utc_offset_valid);
                assert_eq!(data.priority1, 1);
                assert_eq!(data.clock_class, 248);
                assert_eq!(data.variance, 0x4E5D);
                assert_eq!(data.gm_identity, builder().clock_identity);
                assert_eq!(data.steps_removed, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pdelay_resp_roundtrip() {
        let requesting = [1u8; 10];
        let t2 = Timestamp {
            secs: 100,
            nanos: 200,
        };
        let b = builder().pdelay_resp(42, &requesting, t2);
        match parse(&b) {
            Some(Msg::PdelayResp {
                seq,
                req_receipt,
                requesting: r,
                ..
            }) => {
                assert_eq!(seq, 42);
                assert_eq!(req_receipt, t2);
                assert_eq!(r, requesting);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn follow_up_has_gptp_tlv() {
        let b = builder().follow_up(3, Timestamp { secs: 5, nanos: 6 }, -3);
        assert_eq!(b.len(), 76);
        assert_eq!(&b[44..48], &[0x00, 0x03, 0x00, 28]);
        assert_eq!(&b[48..51], &[0x00, 0x80, 0xC2]);
    }

    #[test]
    fn sync_follow_up_parse() {
        let origin = Timestamp {
            secs: 1_700_000_000,
            nanos: 123_456_789,
        };
        match parse(&builder().sync(9, -3)) {
            Some(Msg::Sync {
                seq,
                log_interval,
                two_step,
                ..
            }) => {
                assert_eq!(seq, 9);
                assert_eq!(log_interval, -3);
                assert!(two_step);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let mut fup = builder().follow_up(9, origin, -3);
        fup[58..60].copy_from_slice(&7u16.to_be_bytes());
        // lastGmPhaseChange = -1500 ns, as ScaledNs.
        let scaled = (-1500i128 << 16).to_be_bytes();
        fup[60..72].copy_from_slice(&scaled[4..16]);
        fup[8..16].copy_from_slice(&(250i64 << 16).to_be_bytes());
        match parse(&fup) {
            Some(Msg::FollowUp {
                seq,
                precise_origin,
                correction_ns,
                info: Some(info),
                ..
            }) => {
                assert_eq!(seq, 9);
                assert_eq!(precise_origin, origin);
                assert_eq!(correction_ns, 250);
                assert_eq!(info.gm_time_base_indicator, 7);
                assert_eq!(info.last_gm_phase_change_ns, -1500);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn bmca_ordering() {
        let ours = AnnounceData {
            priority1: 1,
            clock_class: 248,
            clock_accuracy: 0xFE,
            variance: 0xFFFF,
            priority2: 1,
            gm_identity: [0; 8],
            steps_removed: 0,
            time_source: 0xA0,
        };
        let switch = AnnounceData {
            priority1: 246,
            clock_class: 6,
            clock_accuracy: 0x20,
            variance: 0x4000,
            priority2: 1,
            gm_identity: [1; 8],
            steps_removed: 0,
            time_source: 0x20,
        };
        assert!(ours.bmca_key() < switch.bmca_key());
    }
}
