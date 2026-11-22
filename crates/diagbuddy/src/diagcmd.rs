// SPDX-License-Identifier: Apache-2.0
//! Answers the DIAG requests a client sends when diagbuddy is its only peer.
//!
//! In `dci` mode there is no command path to the modem, but the client still
//! arms its log masks with `LOG_CONFIG` (0x73). This module is pure and does
//! no I/O: it takes one decoded DIAG packet (HDLC removed, CRC checked and
//! removed) and returns the reply bytes plus, when the request changed what the
//! client wants, the full new set of wanted log codes.
//!
//! Layouts (little-endian). Ops 0, 1 and 3 were checked against two
//! independent LOG_CONFIG (`0x73`) implementations (op 0 reply 12 bytes, op 1 reply
//! 76 bytes, op 3 reply = request with `status` inserted after `op`). Op 4 has no
//! independent implementation to check against; its layout is unverified against a modem.

use std::collections::{BTreeMap, BTreeSet};

/// Longest request this module will look at; anything longer is a bad command.
pub const MAX_REQUEST: usize = 8192;

const LOG_CONFIG: u8 = 0x73;
const BAD_COMMAND: u8 = 0x13;
/// A bad-command reply echoes at most this many request bytes.
const BAD_ECHO: usize = 16;
/// Items per equipment id that a log code can express (`(equip << 12) | item`).
const ITEMS_PER_EQUIP: u32 = 0x1000;
/// Mask size reported by op 4 when no op 3 has set one for that equipment id.
const DEFAULT_LAST_ITEM: u32 = 0x0FFF;

/// What to send back, and the new subscription if the request changed it.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub response: Vec<u8>,
    /// `Some(full new set)` only when the set of wanted log codes changed.
    pub subscription: Option<BTreeSet<u16>>,
}

#[derive(Default)]
pub struct Responder {
    /// Wanted items per equipment id (0..=15), each item 0..=0xFFF.
    items: BTreeMap<u8, BTreeSet<u16>>,
    /// `last_item` from the most recent op 3 per equipment id; sizes op 4 masks.
    last_item: BTreeMap<u8, u32>,
    /// Refused requests per first request byte (an empty request counts under 0).
    refused: BTreeMap<u8, u64>,
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn header(op: u32) -> Vec<u8> {
    let mut v = vec![LOG_CONFIG, 0, 0, 0];
    v.extend(op.to_le_bytes());
    v.extend(0u32.to_le_bytes()); // status 0
    v
}

impl Responder {
    pub fn new() -> Responder {
        Responder::default()
    }

    /// Count of refused requests per first request byte.
    pub fn refused(&self) -> &BTreeMap<u8, u64> {
        &self.refused
    }

    fn wanted(&self) -> BTreeSet<u16> {
        let mut s = BTreeSet::new();
        for (&equip, items) in &self.items {
            s.extend(items.iter().map(|&i| (u16::from(equip) << 12) | i));
        }
        s
    }

    fn bad(&mut self, request: &[u8]) -> Reply {
        *self.refused.entry(request.first().copied().unwrap_or(0)).or_insert(0) += 1;
        let mut response = vec![BAD_COMMAND];
        response.extend(&request[..request.len().min(BAD_ECHO)]);
        Reply { response, subscription: None }
    }

    /// Handle one decoded DIAG packet. Never panics, whatever the bytes.
    pub fn handle(&mut self, request: &[u8]) -> Reply {
        if request.len() > MAX_REQUEST || request.first() != Some(&LOG_CONFIG) {
            return self.bad(request);
        }
        let Some(op) = u32_at(request, 4) else { return self.bad(request) };
        let reply = match op {
            0 => Some(self.disable()),
            1 => Some(Reply { response: Self::id_ranges(), subscription: None }),
            3 => self.set_mask(request),
            4 => self.get_mask(request),
            _ => None,
        };
        reply.unwrap_or_else(|| self.bad(request))
    }

    fn disable(&mut self) -> Reply {
        let changed = self.items.values().any(|s| !s.is_empty());
        self.items.clear();
        self.last_item.clear();
        Reply { response: header(0), subscription: changed.then(BTreeSet::new) }
    }

    fn id_ranges() -> Vec<u8> {
        let mut v = header(1);
        for equip in 0u32..16 {
            let n: u32 = if equip == 1 || equip == 11 { 0x0FFF } else { 0 };
            v.extend(n.to_le_bytes());
        }
        v
    }

    fn set_mask(&mut self, request: &[u8]) -> Option<Reply> {
        let equip = u8::try_from(u32_at(request, 8)?).ok().filter(|&e| e <= 15)?;
        let last_item = u32_at(request, 12)?;
        let mask = &request[16..];
        // The mask must cover `last_item` bits; a longer one is read only that far.
        if (mask.len() as u64) < u64::from(last_item).div_ceil(8) {
            return None;
        }
        let mut new = BTreeSet::new();
        for (i, byte) in mask.iter().enumerate() {
            for bit in 0..8 {
                let item = (i * 8 + bit) as u32;
                if byte & (1 << bit) != 0 && item < last_item && item < ITEMS_PER_EQUIP {
                    new.insert(item as u16);
                }
            }
        }
        let before = self.wanted();
        self.items.insert(equip, new);
        self.last_item.insert(equip, last_item);
        let after = self.wanted();

        // Echo the request with `status` inserted after `op`.
        let mut out = request[..8].to_vec();
        out.extend(0u32.to_le_bytes());
        out.extend(&request[8..]);
        Some(Reply { response: out, subscription: (after != before).then_some(after) })
    }

    /// The mask in an op 4 reply is `ceil(last_item / 8)` bytes, `last_item`
    /// being the one from the most recent op 3 for that equipment id, or
    /// 0x0FFF if none was set (capped at 0x1000 items).
    fn get_mask(&mut self, request: &[u8]) -> Option<Reply> {
        let equip = u8::try_from(u32_at(request, 8)?).ok().filter(|&e| e <= 15)?;
        let last = self.last_item.get(&equip).copied().unwrap_or(DEFAULT_LAST_ITEM);
        let last = last.min(ITEMS_PER_EQUIP);
        let mut mask = vec![0u8; last.div_ceil(8) as usize];
        if let Some(items) = self.items.get(&equip) {
            for &i in items {
                if let Some(b) = mask.get_mut(usize::from(i) / 8) {
                    *b |= 1 << (i % 8);
                }
            }
        }
        let mut response = header(4);
        response.extend(u32::from(equip).to_le_bytes());
        response.extend(mask);
        Some(Reply { response, subscription: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_req(equip: u32, last_item: u32, bits: &[u32]) -> Vec<u8> {
        let mut req = vec![0x73, 0, 0, 0, 3, 0, 0, 0];
        req.extend(equip.to_le_bytes());
        req.extend(last_item.to_le_bytes());
        let mut mask = vec![0u8; last_item.div_ceil(8) as usize];
        for &b in bits {
            mask[(b / 8) as usize] |= 1 << (b % 8);
        }
        req.extend(mask);
        req
    }

    fn op_req(op: u32, equip: Option<u32>) -> Vec<u8> {
        let mut req = vec![0x73, 0, 0, 0];
        req.extend(op.to_le_bytes());
        if let Some(e) = equip {
            req.extend(e.to_le_bytes());
        }
        req
    }

    #[test]
    fn set_mask_yields_the_codes_and_echoes_with_status() {
        let mut r = Responder::new();
        let mut req = vec![0x73, 0, 0, 0, 3, 0, 0, 0, 0x0B, 0, 0, 0, 0xC1, 0, 0, 0];
        let mut mask = vec![0u8; 25];
        mask[0xC0 / 8] |= 1 << (0xC0 % 8);
        req.extend(&mask);
        let rep = r.handle(&req);
        assert_eq!(rep.subscription, Some(BTreeSet::from([0xB0C0])));
        assert_eq!(&rep.response[..12], &[0x73, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&rep.response[12..], &req[8..]);
    }

    #[test]
    fn a_second_equipment_id_adds_and_the_same_id_replaces() {
        let mut r = Responder::new();
        r.handle(&set_req(0xB, 0x100, &[0xC0]));
        let rep = r.handle(&set_req(0x1, 0x100, &[0x05]));
        assert_eq!(rep.subscription, Some(BTreeSet::from([0xB0C0, 0x1005])));
        let rep = r.handle(&set_req(0xB, 0x100, &[0xC1]));
        assert_eq!(rep.subscription, Some(BTreeSet::from([0xB0C1, 0x1005])));
    }

    #[test]
    fn an_unchanged_set_reports_no_subscription() {
        let mut r = Responder::new();
        r.handle(&set_req(0xB, 0x100, &[0xC0]));
        assert_eq!(r.handle(&set_req(0xB, 0x100, &[0xC0])).subscription, None);
    }

    #[test]
    fn disable_empties_the_set() {
        let mut r = Responder::new();
        r.handle(&set_req(0xB, 0x100, &[0xC0]));
        let rep = r.handle(&op_req(0, None));
        assert_eq!(rep.subscription, Some(BTreeSet::new()));
        assert_eq!(rep.response, [0x73, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.handle(&op_req(0, None)).subscription, None);
    }

    #[test]
    fn get_mask_reflects_the_set() {
        let mut r = Responder::new();
        r.handle(&set_req(0xB, 0x100, &[0xC0, 0x01]));
        let rep = r.handle(&op_req(4, Some(0xB)));
        assert_eq!(&rep.response[..12], &[0x73, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&rep.response[12..16], &[0x0B, 0, 0, 0]);
        let mask = &rep.response[16..];
        assert_eq!(mask.len(), 0x100 / 8);
        assert_eq!(mask[0], 0x02); // item 1
        assert_eq!(mask[0xC0 / 8], 1);
        assert_eq!(rep.subscription, None);
        // Never set: default 0x0FFF items, all clear.
        let rep = r.handle(&op_req(4, Some(3)));
        assert_eq!(rep.response.len(), 16 + 512);
        assert!(rep.response[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn id_ranges_names_equipment_1_and_11() {
        let rep = Responder::new().handle(&op_req(1, None));
        assert_eq!(rep.response.len(), 76);
        assert_eq!(&rep.response[..12], &[0x73, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        for e in 0..16usize {
            let n = u32::from_le_bytes(rep.response[12 + 4 * e..16 + 4 * e].try_into().unwrap());
            assert_eq!(n, if e == 1 || e == 11 { 0x0FFF } else { 0 }, "equip {e}");
        }
        assert_eq!(rep.subscription, None);
    }

    #[test]
    fn truncated_set_mask_is_a_bad_command_and_changes_nothing() {
        let rep = Responder::new().handle(&[0x73, 0, 0, 0, 3, 0, 0, 0, 0x0B]);
        assert_eq!(rep.response[0], 0x13);
        assert_eq!(rep.subscription, None);
    }

    #[test]
    fn unknown_command_is_a_bad_command() {
        let mut r = Responder::new();
        let rep = r.handle(&[0x7D, 1, 2]);
        assert_eq!(rep.response, [0x13, 0x7D, 1, 2]);
        assert_eq!(rep.subscription, None);
        assert_eq!(r.refused().get(&0x7D), Some(&1));
    }

    #[test]
    fn bad_command_echo_is_truncated_to_16_bytes() {
        let rep = Responder::new().handle(&[0x7D; 40]);
        assert_eq!(rep.response.len(), 17);
    }

    #[test]
    fn empty_and_oversize_requests_are_refused() {
        let mut r = Responder::new();
        assert_eq!(r.handle(&[]).response, [0x13]);
        assert_eq!(r.refused().get(&0), Some(&1));
        let mut big = set_req(0xB, 0x100, &[1]);
        big.resize(MAX_REQUEST + 1, 0);
        assert_eq!(r.handle(&big).response[0], 0x13);
        assert_eq!(r.refused().get(&0x73), Some(&1));
    }

    #[test]
    fn unknown_op_and_bad_equipment_are_refused() {
        let mut r = Responder::new();
        assert_eq!(r.handle(&op_req(2, None)).response[0], 0x13);
        assert_eq!(r.handle(&op_req(4, Some(16))).response[0], 0x13);
        assert_eq!(r.handle(&op_req(4, None)).response[0], 0x13);
        assert_eq!(r.handle(&set_req(16, 8, &[1])).response[0], 0x13);
        assert!(r.wanted().is_empty());
    }

    #[test]
    fn mask_longer_than_last_item_is_ignored_past_the_end() {
        let mut r = Responder::new();
        // last_item 4 but the mask has bits 2, 5 and 9 set.
        let mut req = set_req(0x1, 4, &[]);
        req.truncate(16);
        req.extend([0b0010_0100, 0b0000_0010]);
        let rep = r.handle(&req);
        assert_eq!(rep.subscription, Some(BTreeSet::from([0x1002])));
        assert_eq!(&rep.response[12..], &req[8..]);
    }

    #[test]
    fn items_above_0xfff_are_ignored() {
        let mut r = Responder::new();
        let rep = r.handle(&set_req(0x2, 0x1010, &[0x0FFF, 0x1000, 0x1005]));
        assert_eq!(rep.subscription, Some(BTreeSet::from([0x2FFF])));
    }

    #[test]
    fn every_prefix_of_a_set_mask_request_is_refused_cleanly() {
        let full = set_req(0xB, 0xC1, &[0xC0]);
        for n in 0..full.len() {
            let mut r = Responder::new();
            let rep = r.handle(&full[..n]);
            assert_eq!(rep.response[0], 0x13, "prefix {n}");
            assert_eq!(rep.subscription, None, "prefix {n}");
            assert!(r.wanted().is_empty(), "prefix {n}");
        }
        assert!(Responder::new().handle(&full).subscription.is_some());
    }

    #[test]
    fn pseudo_random_garbage_never_panics_and_gets_a_well_formed_reply() {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut r = Responder::new();
        for case in 0..600 {
            let len = (next() % 40) as usize;
            let mut req: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if case % 3 != 0 && req.len() >= 8 {
                // Steer into LOG_CONFIG with a small op so the parsers run.
                req[..4].copy_from_slice(&[0x73, 0, 0, 0]);
                req[4..7].fill(0);
                req[7] = (next() % 6) as u8;
            }
            let before = r.wanted();
            let rep = r.handle(&req);
            assert!(matches!(rep.response.first(), Some(0x73 | 0x13)), "case {case}");
            if rep.response[0] == 0x13 {
                assert_eq!(rep.subscription, None, "case {case}");
                assert_eq!(r.wanted(), before, "case {case}");
                assert!(rep.response.len() <= 17);
            }
        }
    }
}
