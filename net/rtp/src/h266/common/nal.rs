//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H266 NAL unit header (RFC 9328 §1.1.4, ITU-T H.266 Table 5).

use super::NAL_HEADER_SIZE;

// H266 NAL unit types (ITU-T H.266 Table 5).
pub(crate) const NAL_TYPE_IDR_W_RADL: u8 = 7;
pub(crate) const NAL_TYPE_CRA: u8 = 9;
pub(crate) const NAL_TYPE_VPS: u8 = 14;
pub(crate) const NAL_TYPE_SPS: u8 = 15;
pub(crate) const NAL_TYPE_PPS: u8 = 16;
pub(crate) const NAL_TYPE_AUD: u8 = 20;
pub(crate) const NAL_TYPE_FD: u8 = 25;

/// Parsed H266 two-byte NAL unit header.
///
/// ```text
/// +---------------+---------------+
/// |0|1|2|3|4|5|6|7|0|1|2|3|4|5|6|7|
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |F|Z|  LayerID  |  Type   | TID |
/// +---------------+---------------+
/// ```
///
/// `Z` (`nuh_reserved_zero_bit`) is reserved (0) and is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NalHeader {
    /// forbidden_zero_bit (F).
    pub forbidden: bool,
    /// nuh_layer_id (6 bits).
    pub layer_id: u8,
    /// nal_unit_type (5 bits, in the second byte).
    pub nal_type: u8,
    /// nuh_temporal_id_plus1 (3 bits).
    pub tid: u8,
}

impl NalHeader {
    /// Parse the header from the first two bytes of `data`, or `None` if it
    /// is shorter than a NAL header.
    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < NAL_HEADER_SIZE {
            return None;
        }
        Some(Self::from_bytes(data[0], data[1]))
    }

    pub(crate) fn from_bytes(b0: u8, b1: u8) -> Self {
        NalHeader {
            forbidden: b0 & 0b1000_0000 != 0,
            layer_id: b0 & 0b0011_1111,
            nal_type: (b1 >> 3) & 0b1_1111,
            tid: b1 & 0b111,
        }
    }

    pub(crate) fn to_bytes(self) -> [u8; 2] {
        [
            ((self.forbidden as u8) << 7) | (self.layer_id & 0b0011_1111),
            ((self.nal_type & 0b1_1111) << 3) | (self.tid & 0b111),
        ]
    }

    /// IRAP (keyframe) picture: IDR_W_RADL(7), IDR_N_LP(8), CRA(9).
    pub(crate) fn is_irap(self) -> bool {
        (NAL_TYPE_IDR_W_RADL..=NAL_TYPE_CRA).contains(&self.nal_type)
    }

    /// AUD or filler data (dropped on payloading).
    pub(crate) fn is_aud_or_filler(self) -> bool {
        matches!(self.nal_type, NAL_TYPE_AUD | NAL_TYPE_FD)
    }

    /// VCL NAL unit (coded slice): types 0..=11 (ITU-T H.266 Table 5).
    pub(crate) fn is_vcl(self) -> bool {
        self.nal_type <= 11
    }
}

/// Iterator over the Annex-B NAL units (without their start codes) in `data`.
/// Accepts both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes.
pub(crate) fn iter_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut positions = vec![];
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                positions.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                positions.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    let len = data.len();
    let n = positions.len();
    (0..n).map(move |k| {
        let (off, sc_len) = positions[k];
        let start = off + sc_len;
        let end = if k + 1 < n { positions[k + 1].0 } else { len };
        &data[start..end]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_header_roundtrip() {
        // VPS (type 14), layer 0, tid 1.
        let h = NalHeader::from_bytes(0x00, 0x71);
        assert!(!h.forbidden);
        assert_eq!(h.layer_id, 0);
        assert_eq!(h.nal_type, NAL_TYPE_VPS);
        assert_eq!(h.tid, 1);
        assert_eq!(h.to_bytes(), [0x00, 0x71]);

        // F set, layer 5, type IDR_W_RADL (7), tid 3.
        let h = NalHeader {
            forbidden: true,
            layer_id: 5,
            nal_type: NAL_TYPE_IDR_W_RADL,
            tid: 3,
        };
        let bytes = h.to_bytes();
        assert_eq!(NalHeader::from_bytes(bytes[0], bytes[1]), h);
    }

    #[test]
    fn nal_header_classification() {
        for ty in [NAL_TYPE_IDR_W_RADL, 8, NAL_TYPE_CRA] {
            let h = NalHeader::from_bytes(0, ty << 3);
            assert!(h.is_irap(), "type {ty} is IRAP");
        }
        // GDR (10) is not IRAP.
        assert!(!NalHeader::from_bytes(0, 10 << 3).is_irap());

        assert!(NalHeader::from_bytes(0, NAL_TYPE_AUD << 3).is_aud_or_filler());
        assert!(NalHeader::from_bytes(0, NAL_TYPE_FD << 3).is_aud_or_filler());

        // VCL = types 0..=11.
        for ty in [0u8, 7, 11] {
            assert!(
                NalHeader::from_bytes(0, ty << 3).is_vcl(),
                "type {ty} is VCL"
            );
        }
        for ty in [NAL_TYPE_VPS, NAL_TYPE_AUD, 12] {
            assert!(
                !NalHeader::from_bytes(0, ty << 3).is_vcl(),
                "type {ty} not VCL"
            );
        }
    }

    #[test]
    fn parse_too_short() {
        assert!(NalHeader::parse(&[0x00]).is_none());
        assert!(NalHeader::parse(&[]).is_none());
        assert!(NalHeader::parse(&[0x00, 0x71]).is_some());
    }

    #[test]
    fn iter_nals_mixed_start_codes() {
        // [00 00 00 01] A [00 00 01] BB [00 00 00 01] CCC
        let data = [
            0, 0, 0, 1, 0xAA, //
            0, 0, 1, 0xBB, 0xBB, //
            0, 0, 0, 1, 0xCC, 0xCC, 0xCC,
        ];
        let nals: Vec<&[u8]> = iter_nals(&data).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0xAA]);
        assert_eq!(nals[1], &[0xBB, 0xBB]);
        assert_eq!(nals[2], &[0xCC, 0xCC, 0xCC]);
    }

    #[test]
    fn iter_nals_no_start_code() {
        assert_eq!(iter_nals(&[0xAA, 0xBB]).count(), 0);
    }
}
