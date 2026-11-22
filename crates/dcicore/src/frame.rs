// SPDX-License-Identifier: Apache-2.0
//! Re-frame a DCI log record as ordinary DIAG: optional `0x98` header, a `0x10` log
//! packet, CRC-16 FCS, HDLC escaping, trailing `0x7E`. That is the shape of rayhunter's own
//! stream on the FX2000 (`98 01 00 00 01 00 00 00 10 00 …`), so existing readers take it as is.

/// Used when the stack sent no `0x98` header for a record (it sends one for some records
/// and not others). `[0]=0x98 [1]=radio_id [2:4]=pad [4:8]=tx_mask`, values as observed.
pub const DEFAULT_MULTI_RADIO: [u8; 8] = [0x98, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];

pub const LOG_CMD: u8 = 0x10;
const FLAG: u8 = 0x7E;
const ESC: u8 = 0x7D;

/// CRC-16/X-25, the HDLC frame check sequence DIAG uses. Appended little-endian.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x8408 } else { crc >> 1 };
        }
    }
    !crc
}

/// HDLC-encode one packet (escape, FCS, trailing flag) onto `out`.
pub fn hdlc_encode(pkt: &[u8], out: &mut Vec<u8>) {
    let fcs = crc16(pkt).to_le_bytes();
    for &b in pkt.iter().chain(fcs.iter()) {
        if b == FLAG || b == ESC {
            out.push(ESC);
            out.push(b ^ 0x20);
        } else {
            out.push(b);
        }
    }
    out.push(FLAG);
}

/// Build the unescaped DIAG packet for one log item (`log` starts at its `u16 len`).
pub fn log_packet(log: &[u8], header: Option<&[u8; 8]>) -> Vec<u8> {
    let mut p = Vec::with_capacity(log.len() + 12);
    if let Some(h) = header {
        p.extend_from_slice(h);
    }
    p.push(LOG_CMD);
    p.push(0); // "more" byte
    p.extend_from_slice(&(log.len() as u16).to_le_bytes());
    p.extend_from_slice(log);
    p
}

/// Whether to put the `0x98` header in front of each record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outer {
    /// Use the stack's header when it sent one, otherwise [`DEFAULT_MULTI_RADIO`].
    MultiRadio,
    /// Bare `0x10` log packets.
    None,
}

/// One record, fully framed.
pub fn frame_log(log: &[u8], stack_header: Option<&[u8; 8]>, outer: Outer) -> Vec<u8> {
    let header = match outer {
        Outer::None => None,
        Outer::MultiRadio => Some(match stack_header {
            Some(h) if h[0] == 0x98 => h,
            _ => &DEFAULT_MULTI_RADIO,
        }),
    };
    let pkt = log_packet(log, header);
    let mut out = Vec::with_capacity(pkt.len() + pkt.len() / 16 + 4);
    hdlc_encode(&pkt, &mut out);
    out
}

/// Undo [`hdlc_encode`]: strip the flag, unescape, check the FCS. `None` on any mismatch.
pub fn hdlc_decode(frame: &[u8]) -> Option<Vec<u8>> {
    let body = frame.strip_suffix(&[FLAG])?;
    let mut raw = Vec::new();
    let mut it = body.iter();
    while let Some(&b) = it.next() {
        raw.push(if b == ESC { it.next()? ^ 0x20 } else { b });
    }
    if raw.len() < 2 {
        return None;
    }
    let (pkt, fcs) = raw.split_at(raw.len() - 2);
    (crc16(pkt).to_le_bytes() == fcs).then(|| pkt.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc16(b"123456789"), 0x906E); // CRC-16/X-25 catalogue check value
    }

    #[test]
    fn escapes_flag_and_escape_bytes() {
        let mut out = Vec::new();
        hdlc_encode(&[0x7E, 0x01, 0x7D], &mut out);
        assert_eq!(&out[..5], &[0x7D, 0x5E, 0x01, 0x7D, 0x5D]);
        assert_eq!(*out.last().unwrap(), 0x7E);
        assert_eq!(out.iter().filter(|&&b| b == 0x7E).count(), 1);
        assert_eq!(hdlc_decode(&out).unwrap(), vec![0x7E, 0x01, 0x7D]);
    }

    fn sample_log() -> Vec<u8> {
        let mut log = 15u16.to_le_bytes().to_vec();
        log.extend_from_slice(&0xB063u16.to_le_bytes());
        log.extend_from_slice(&0x7E7D_0000_0000_0001u64.to_le_bytes());
        log.extend_from_slice(&[0xAA, 0x7E, 0xBB]);
        log
    }

    #[test]
    fn frames_with_the_default_multi_radio_header() {
        let log = sample_log();
        let pkt = hdlc_decode(&frame_log(&log, None, Outer::MultiRadio)).unwrap();
        assert_eq!(&pkt[..8], &DEFAULT_MULTI_RADIO);
        assert_eq!(&pkt[8..12], &[0x10, 0x00, 15, 0]);
        assert_eq!(&pkt[12..], &log[..]);
    }

    #[test]
    fn keeps_the_stack_header_and_rejects_a_foreign_one() {
        let log = sample_log();
        let h = [0x98, 2, 0, 0, 3, 0, 0, 0];
        let pkt = hdlc_decode(&frame_log(&log, Some(&h), Outer::MultiRadio)).unwrap();
        assert_eq!(&pkt[..8], &h);
        let bad = [0x11; 8];
        let pkt = hdlc_decode(&frame_log(&log, Some(&bad), Outer::MultiRadio)).unwrap();
        assert_eq!(&pkt[..8], &DEFAULT_MULTI_RADIO);
    }

    #[test]
    fn bare_framing_starts_with_the_log_command() {
        let log = sample_log();
        let pkt = hdlc_decode(&frame_log(&log, None, Outer::None)).unwrap();
        assert_eq!(pkt[0], 0x10);
        assert_eq!(pkt.len(), 4 + log.len());
    }
}
