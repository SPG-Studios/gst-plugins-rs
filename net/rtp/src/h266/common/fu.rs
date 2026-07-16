//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H266 Fragmentation Unit header (RFC 9328 §4.3.3).

/// FU header byte (RFC 9328 §4.3.3): `S(1) | E(1) | P(1) | FuType(5)`.
///
/// ```text
/// +---------------+
/// |0|1|2|3|4|5|6|7|
/// +-+-+-+-+-+-+-+-+
/// |S|E|P|  FuType |
/// +---------------+
/// ```
///
/// The P bit, when set to 1, indicates the last FU of the last VCL NAL unit of
/// a coded picture (RFC 9328 §4.3.3). It is an optional end-of-picture hint;
/// this payloader does not set it (always 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FuHeader {
    pub start: bool,
    pub end: bool,
    pub last_picture: bool,
    pub fu_type: u8,
}

impl FuHeader {
    pub(crate) fn new(start: bool, end: bool, fu_type: u8) -> Self {
        FuHeader {
            start,
            end,
            last_picture: false,
            fu_type,
        }
    }

    pub(crate) fn parse(b: u8) -> Self {
        FuHeader {
            start: b & 0b1000_0000 != 0,
            end: b & 0b0100_0000 != 0,
            last_picture: b & 0b0010_0000 != 0,
            fu_type: b & 0b1_1111,
        }
    }

    pub(crate) fn to_byte(self) -> u8 {
        let mut b = self.fu_type & 0b1_1111;
        if self.start {
            b |= 0b1000_0000;
        }
        if self.end {
            b |= 0b0100_0000;
        }
        if self.last_picture {
            b |= 0b0010_0000;
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fu_header_roundtrip() {
        let cases = [
            (0x87u8, true, false, false, 7),
            (0x47, false, true, false, 7),
            (0x27, false, false, true, 7),
            (0x60, false, true, true, 0),
            (0x9f, true, false, false, 31),
            (0x09, false, false, false, 9),
        ];
        for (byte, s, e, p, ty) in cases {
            let h = FuHeader::parse(byte);
            assert_eq!(h.start, s);
            assert_eq!(h.end, e);
            assert_eq!(h.last_picture, p);
            assert_eq!(h.fu_type, ty);
            assert_eq!(h.to_byte(), byte);
        }
    }

    #[test]
    fn fu_header_new_never_sets_p() {
        assert!(!FuHeader::new(true, false, 9).last_picture);
        assert_eq!(FuHeader::new(true, true, 7).to_byte(), 0xc7);
    }
}
