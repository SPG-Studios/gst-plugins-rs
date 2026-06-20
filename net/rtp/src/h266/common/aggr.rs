//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H266 Aggregation Packet (type 28) building and parsing (RFC 9328 §4.3.2).
//!
//! The 2-byte PayloadHdr is an ordinary NAL unit header with Type=28; each
//! aggregated unit is prefixed by a 16-bit size. The DONL field is omitted
//! (this implementation operates with `sprop-max-don-diff = 0`).
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |   PayloadHdr (Type=28)        |          NALU 1 Size          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |          NALU 1 HDR           |           NALU 1 Data ...     |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |              ...              |          NALU n Size          |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |          NALU n HDR           |           NALU n Data ...     |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

use super::{NAL_HEADER_SIZE, RTP_TYPE_AP, nal::NalHeader};

/// Build an AP from `units`, deriving the PayloadHdr per RFC 9328 §4.3.2:
/// F = OR of the aggregated units' F bits, LayerId/TID = lowest of the
/// aggregated units. Returns `None` if fewer than 2 units, a unit is
/// malformed or exceeds the 16-bit length field, or the AP would exceed
/// `max_payload`.
pub(crate) fn build_ap(units: &[&[u8]], max_payload: usize) -> Option<Vec<u8>> {
    if units.len() < 2 {
        return None;
    }
    if units
        .iter()
        .any(|u| u.len() < NAL_HEADER_SIZE || u.len() > u16::MAX as usize)
    {
        return None;
    }
    let total = NAL_HEADER_SIZE + units.iter().map(|u| 2 + u.len()).sum::<usize>();
    if total > max_payload {
        return None;
    }

    let mut f_bit = false;
    let mut min_layer = 0b0011_1111u8;
    let mut min_tid = 0b111u8;
    for u in units {
        let h = NalHeader::from_bytes(u[0], u[1]);
        f_bit |= h.forbidden;
        min_layer = min_layer.min(h.layer_id);
        min_tid = min_tid.min(h.tid);
    }
    // TID is nuh_temporal_id_plus1: never emit 0.
    let tid = min_tid.max(1);

    let hdr = NalHeader {
        forbidden: f_bit,
        layer_id: min_layer,
        nal_type: RTP_TYPE_AP,
        tid,
    };

    let mut ap = Vec::with_capacity(total);
    ap.extend_from_slice(&hdr.to_bytes());
    for u in units {
        ap.extend_from_slice(&(u.len() as u16).to_be_bytes());
        ap.extend_from_slice(u);
    }
    Some(ap)
}

/// Iterate the aggregated NAL units (without start codes) inside an AP
/// payload. The 2-byte AP PayloadHdr is skipped; each unit is `[size16][nal]`.
/// Stops at the first malformed length.
pub(crate) fn iter_ap_units(payload: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut offset = NAL_HEADER_SIZE;
    std::iter::from_fn(move || {
        if offset + 2 > payload.len() {
            return None;
        }
        let size = ((payload[offset] as usize) << 8) | payload[offset + 1] as usize;
        offset += 2;
        if size < NAL_HEADER_SIZE || offset + size > payload.len() {
            return None;
        }
        let nal = &payload[offset..offset + size];
        offset += size;
        Some(nal)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(ty: u8, layer: u8, tid: u8, len: usize) -> Vec<u8> {
        let mut n = vec![layer & 0x3f, (ty << 3) | (tid & 0x7)];
        n.resize(len, 0xAB);
        n
    }

    #[test]
    fn build_ap_lowest_layer_tid_and_f_or() {
        let a = nal(15, 2, 3, 8); // layer 2, tid 3
        let mut b = nal(16, 4, 1, 6); // layer 4, tid 1
        b[0] |= 0x80; // F set on one unit
        let ap = build_ap(&[&a, &b], 1400).unwrap();

        let hdr = NalHeader::from_bytes(ap[0], ap[1]);
        assert_eq!(hdr.nal_type, RTP_TYPE_AP);
        assert_eq!(hdr.layer_id, 2, "lowest layer");
        assert_eq!(hdr.tid, 1, "lowest tid");
        assert!(hdr.forbidden, "F = OR of units");
    }

    #[test]
    fn build_ap_rejects_under_two_or_too_big() {
        let a = nal(15, 0, 1, 8);
        assert!(build_ap(&[&a], 1400).is_none(), "needs >= 2 units");
        let b = nal(16, 0, 1, 8);
        assert!(build_ap(&[&a, &b], 10).is_none(), "exceeds max_payload");
    }

    #[test]
    fn ap_roundtrip() {
        let a = nal(14, 0, 1, 8);
        let b = nal(15, 0, 1, 12);
        let c = nal(16, 0, 1, 6);
        let ap = build_ap(&[&a, &b, &c], 1400).unwrap();
        let units: Vec<&[u8]> = iter_ap_units(&ap).collect();
        assert_eq!(units, vec![a.as_slice(), b.as_slice(), c.as_slice()]);
    }

    #[test]
    fn iter_ap_units_stops_on_truncation() {
        // AP header + one valid unit + a truncated size.
        let a = nal(15, 0, 1, 4);
        let mut ap = vec![0u8, (RTP_TYPE_AP << 3) | 1];
        ap.extend_from_slice(&(a.len() as u16).to_be_bytes());
        ap.extend_from_slice(&a);
        ap.extend_from_slice(&[0x00, 0xff, 0x01]); // claims 255 bytes
        let units: Vec<&[u8]> = iter_ap_units(&ap).collect();
        assert_eq!(units, vec![a.as_slice()]);
    }
}
