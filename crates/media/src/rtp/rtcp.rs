//! Minimal RTCP — Sender Report (SR) and Receiver Report (RR) framing.
//!
//! Port of `../sipjsserver/src/media/rtp/rtcp.ts`. First-cut scope is *counts
//! only*: we generate SR/RR on the standard interval and report aggregated
//! packet/octet counts via stats; we do not assert SR field contents yet. So
//! this builds well-formed SR/RR with no report blocks and exposes a demux
//! helper to tell RTCP from RTP on a muxed port (RFC 5761).

pub const RTCP_PT_SR: u8 = 200;
pub const RTCP_PT_RR: u8 = 201;

/// Seconds between the 1900 NTP epoch and the 1970 Unix epoch.
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

#[derive(Debug, Clone, Copy)]
pub struct SenderReportFields {
    pub ssrc: u32,
    /// Wall/virtual time in ms; split into NTP msw/lsw.
    pub ntp_ms: i64,
    pub rtp_timestamp: u32,
    pub packet_count: u32,
    pub octet_count: u32,
}

fn ntp_from_ms(ms: i64) -> (u32, u32) {
    let ms = ms.max(0) as u64;
    let seconds = (ms / 1000) + NTP_EPOCH_OFFSET;
    let fraction = (((ms % 1000) as f64 / 1000.0) * 4_294_967_296.0) as u64;
    (seconds as u32, fraction as u32)
}

/// Build a Sender Report with no report blocks (28 bytes).
pub fn encode_sender_report(f: &SenderReportFields) -> Vec<u8> {
    let mut buf = vec![0u8; 28];
    buf[0] = 2 << 6; // V=2, P=0, RC=0
    buf[1] = RTCP_PT_SR;
    buf[2..4].copy_from_slice(&((28u16 / 4) - 1).to_be_bytes()); // length in words - 1
    buf[4..8].copy_from_slice(&f.ssrc.to_be_bytes());
    let (msw, lsw) = ntp_from_ms(f.ntp_ms);
    buf[8..12].copy_from_slice(&msw.to_be_bytes());
    buf[12..16].copy_from_slice(&lsw.to_be_bytes());
    buf[16..20].copy_from_slice(&f.rtp_timestamp.to_be_bytes());
    buf[20..24].copy_from_slice(&f.packet_count.to_be_bytes());
    buf[24..28].copy_from_slice(&f.octet_count.to_be_bytes());
    buf
}

/// Build a Receiver Report with no report blocks (8 bytes).
pub fn encode_receiver_report(ssrc: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0] = 2 << 6;
    buf[1] = RTCP_PT_RR;
    buf[2..4].copy_from_slice(&((8u16 / 4) - 1).to_be_bytes());
    buf[4..8].copy_from_slice(&ssrc.to_be_bytes());
    buf
}

/// RFC 5761 muxed-port demux: is this datagram RTCP (vs RTP)?
pub fn is_rtcp(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && (200..=204).contains(&bytes[1])
}

/// The RTCP packet type (second octet), for counting.
pub fn rtcp_packet_type(bytes: &[u8]) -> i16 {
    if bytes.len() >= 2 {
        bytes[1] as i16
    } else {
        -1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sr_is_well_formed() {
        let sr = encode_sender_report(&SenderReportFields {
            ssrc: 0x1234_5678,
            ntp_ms: 1000,
            rtp_timestamp: 160,
            packet_count: 5,
            octet_count: 800,
        });
        assert_eq!(sr.len(), 28);
        assert_eq!(sr[0] >> 6, 2); // version
        assert_eq!(sr[1], RTCP_PT_SR);
        assert!(is_rtcp(&sr));
        assert_eq!(u32::from_be_bytes([sr[4], sr[5], sr[6], sr[7]]), 0x1234_5678);
        assert_eq!(u32::from_be_bytes([sr[20], sr[21], sr[22], sr[23]]), 5);
    }

    #[test]
    fn rr_is_well_formed_and_rtcp() {
        let rr = encode_receiver_report(0xABCD);
        assert_eq!(rr.len(), 8);
        assert_eq!(rr[1], RTCP_PT_RR);
        assert!(is_rtcp(&rr));
    }

    #[test]
    fn rtp_payload_types_are_not_rtcp() {
        // PCMA (PT 8) second octet without marker is 8 — clearly < 200.
        assert!(!is_rtcp(&[0x80, 8, 0, 0]));
        assert!(!is_rtcp(&[0x80, 0, 0, 0])); // PCMU PT 0
    }
}
