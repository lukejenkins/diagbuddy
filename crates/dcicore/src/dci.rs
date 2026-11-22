// SPDX-License-Identifier: Apache-2.0
//! The DCI codec: request numbers, the subscription message, and the data-read layout.
//!
//! Every constant and layout here was observed on a device (FX2000
//! and M2000B on kernel `/dev/diag`, FX3100 on the userspace diag-router). They are
//! the same on both transports; only how a request reaches the stack differs.

/// Request numbers. On the kernel they are ioctl numbers; on the router they are the
/// leading `u32` of a request message.
pub const OP_DEINIT: u32 = 21;
pub const OP_SUPPORT: u32 = 22;
pub const OP_REGISTER: u32 = 23;

/// Size of the (all-zero) kernel register struct and of the support-query struct.
pub const REGISTER_LEN: usize = 14;
pub const SUPPORT_LEN: usize = 6;

/// The diag-router's register body: `[i32 pid][i32 client = -1][u16 notify][i32 signal]
/// [i32 token][u16 pad]`, 20 bytes. Traced from the vendor's own DCI client on an FX4200.
/// The router keys its client table on the pid: an all-zero
/// body (pid 0) registered once or twice per router lifetime and was then refused with
/// 1002, while the vendor client kept registering. dcitap asks for no status signal
/// (`notify` and `signal` zero; the vendor sample sends 7 and 18).
pub fn router_register_body(pid: u32) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..4].copy_from_slice(&pid.to_le_bytes());
    b[4..8].copy_from_slice(&(-1i32).to_le_bytes());
    b
}

/// First `i32` of a subscription write and of a data read.
pub const DCI_DATA: i32 = 0x40;
/// Record tags inside a data read.
pub const REC_LOG: i32 = -1;
pub const REC_HEADER: i32 = -3;

/// Smallest log record: `u16 len, u16 code, u64 ts`.
pub const LOG_HEADER_LEN: usize = 12;

/// The log-subscription write: `[0x40][-1][client][set][n][u16 code; n]`.
/// Both forms are measured on the FX4200 (diag-router): `set = true` subscribes, and
/// `set = false` (unsubscribe, used for shedding and teardown) took the router's CPU back to
/// idle in phase 1A step 9. dcitap also drops a shed code in software.
pub fn log_mask_msg(client: i32, set: bool, codes: &[u16]) -> Vec<u8> {
    let mut w = Vec::with_capacity(20 + 2 * codes.len());
    for v in [DCI_DATA, -1, client, set as i32, codes.len() as i32] {
        w.extend_from_slice(&v.to_le_bytes());
    }
    for c in codes {
        w.extend_from_slice(&c.to_le_bytes());
    }
    w
}

/// One log record from a data read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record<'a> {
    /// The 8-byte `0x98` header that preceded this record, if the stack sent one.
    pub header: Option<[u8; 8]>,
    pub code: u16,
    /// The whole log item, starting at its `u16 len` (so `log.len() == len`).
    pub log: &'a [u8],
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub records: Vec<Record<'a>>,
    /// Bytes left over that did not parse as records (unknown tag or truncated record).
    pub unparsed_bytes: usize,
}

fn le_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// The leading `i32` of a read, or `None` if the read is shorter than 4 bytes.
pub fn read_type(b: &[u8]) -> Option<i32> {
    (b.len() >= 4).then(|| le_i32(b, 0))
}

/// Parse a data read: `[i32 0x40][i32 token][i32 total]` then records, each an optional
/// `[i32 -3][8-byte 0x98 header]` followed by `[i32 -1][u16 len][u16 code][u64 ts][payload]`.
/// Returns `None` for a read that is not a data read.
pub fn parse_data(b: &[u8]) -> Option<Parsed<'_>> {
    if read_type(b)? != DCI_DATA || b.len() < 12 {
        return None;
    }
    let mut out = Parsed::default();
    let mut p = 12usize;
    let mut header: Option<[u8; 8]> = None;
    while p < b.len() {
        if p + 4 > b.len() {
            break;
        }
        match le_i32(b, p) {
            REC_HEADER if p + 12 <= b.len() => {
                let mut h = [0u8; 8];
                h.copy_from_slice(&b[p + 4..p + 12]);
                header = Some(h);
                p += 12;
            }
            REC_LOG if p + 4 + LOG_HEADER_LEN <= b.len() => {
                let len = u16::from_le_bytes([b[p + 4], b[p + 5]]) as usize;
                if len < LOG_HEADER_LEN || p + 4 + len > b.len() {
                    break;
                }
                let log = &b[p + 4..p + 4 + len];
                let code = u16::from_le_bytes([log[2], log[3]]);
                out.records.push(Record { header: header.take(), code, log });
                p += 4 + len;
            }
            _ => break,
        }
    }
    out.unparsed_bytes = b.len() - p;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_rec(code: u16, ts: u64, payload: &[u8]) -> Vec<u8> {
        let mut r = (-1i32).to_le_bytes().to_vec();
        r.extend_from_slice(&((LOG_HEADER_LEN + payload.len()) as u16).to_le_bytes());
        r.extend_from_slice(&code.to_le_bytes());
        r.extend_from_slice(&ts.to_le_bytes());
        r.extend_from_slice(payload);
        r
    }

    fn data_read(body: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [DCI_DATA, 7, body.len() as i32] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn subscription_message_matches_the_probe_layout() {
        let m = log_mask_msg(1, true, &[0xB114, 0xB193]);
        assert_eq!(
            m,
            [
                0x40, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0x14,
                0xB1, 0x93, 0xB1
            ]
        );
        assert_eq!(&log_mask_msg(3, false, &[0xB063])[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn parses_records_with_and_without_a_0x98_header() {
        let mut body = log_rec(0xB063, 0x1122, &[1, 2, 3]);
        body.extend_from_slice(&(-3i32).to_le_bytes());
        body.extend_from_slice(&[0x98, 1, 0, 0, 1, 0, 0, 0]);
        body.extend_from_slice(&log_rec(0xB064, 0x3344, &[]));
        let read = data_read(&body);
        let p = parse_data(&read).unwrap();
        assert_eq!(p.unparsed_bytes, 0);
        assert_eq!(p.records.len(), 2);
        assert_eq!(p.records[0].code, 0xB063);
        assert_eq!(p.records[0].header, None);
        assert_eq!(p.records[0].log.len(), 15);
        assert_eq!(p.records[1].code, 0xB064);
        assert_eq!(p.records[1].header, Some([0x98, 1, 0, 0, 1, 0, 0, 0]));
    }

    #[test]
    fn a_truncated_record_is_counted_not_emitted() {
        let mut body = log_rec(0xB063, 1, &[9; 4]);
        let whole = body.len();
        body.extend_from_slice(&log_rec(0xB064, 2, &[9; 20])[..10]);
        let read = data_read(&body);
        let p = parse_data(&read).unwrap();
        assert_eq!(p.records.len(), 1);
        assert_eq!(p.unparsed_bytes, read.len() - 12 - whole);
    }

    #[test]
    fn a_length_below_the_log_header_stops_parsing() {
        let mut rec = log_rec(0xB063, 1, &[]);
        rec[4] = 4; // len 4 < 12
        let read = data_read(&rec);
        let p = parse_data(&read).unwrap();
        assert!(p.records.is_empty());
        assert_eq!(p.unparsed_bytes, rec.len());
    }

    #[test]
    fn non_data_reads_are_rejected() {
        assert!(parse_data(&[0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(parse_data(&[0x40, 0]).is_none());
    }
}

#[cfg(test)]
mod router_register_tests {
    use super::*;

    #[test]
    fn the_body_matches_the_traced_vendor_client_apart_from_the_signal_fields() {
        // Vendor trace, pid 31688: c8 7b 00 00 | ff ff ff ff | 07 00 | 12 00 00 00 | 0 x 6.
        let mut want = [0xc8, 0x7b, 0, 0, 0xff, 0xff, 0xff, 0xff, 0x07, 0, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        want[8] = 0; // notify: none asked for
        want[10] = 0; // signal: none asked for
        assert_eq!(router_register_body(31688), want);
    }
}
