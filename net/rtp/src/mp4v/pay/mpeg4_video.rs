// GStreamer RTP MPEG-4 part 2 Video Elementary Stream Payloading - Packet Parser
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use smallvec::SmallVec;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PacketType {
    VideoObject(u8),
    VideoObjectLayer(u8),
    FgsBp(u8),
    VisualObjectSequenceStart(u8),
    VisualObjectSequenceEnd,
    UserData,
    GroupOfVop,
    VideoSessionError,
    VisualObject,
    Vop(VopCodingType),
    Slice,
    Extension,
    FgsVop,
    FbaObject,
    FbaObjectPlane,
    MeshObject,
    MeshObjectPlane,
    StillTextureObject,
    TextureSpatialLayer,
    TextureSnrLayer,
    TextureTile,
    TextureShapeLayer,
    Stuffing,
    Unknown(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VopCodingType {
    I,
    P,
    B,
    S,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Packet {
    ptype: PacketType,
    offset: u32,
    len: u32,
}

impl Packet {
    pub(crate) fn new(offset: usize, len: usize, data: &[u8]) -> Self {
        assert!(data.starts_with(&[0u8, 0, 1]) && data.len() >= 4);
        assert!(offset <= u32::MAX as usize);
        assert!(len <= u32::MAX as usize);

        // ISO 14496-2:2004 Table 6-3 p57
        let ptype = match data[3] {
            n @ 0x00..=0x1f => PacketType::VideoObject(n),
            n @ 0x20..=0x3f => PacketType::VideoObjectLayer(n),
            n @ 0x40..=0x5f => PacketType::FgsBp(n),
            0xb0 if data.len() >= 5 => PacketType::VisualObjectSequenceStart(data[4]),
            0xb1 => PacketType::VisualObjectSequenceEnd,
            0xb2 => PacketType::UserData,
            0xb3 => PacketType::GroupOfVop,
            0xb4 => PacketType::VideoSessionError,
            0xb5 => PacketType::VisualObject,
            0xb6 if data.len() >= 5 => {
                let coding_type = match data[4] >> 6 {
                    0b00 => VopCodingType::I,
                    0b01 => VopCodingType::P,
                    0b10 => VopCodingType::B,
                    0b11 => VopCodingType::S,
                    _ => unreachable!(),
                };
                PacketType::Vop(coding_type)
            }
            0xb7 => PacketType::Slice,
            0xb8 => PacketType::Extension,
            0xb9 => PacketType::FgsVop,
            0xba => PacketType::FbaObject,
            0xbb => PacketType::FbaObjectPlane,
            0xbc => PacketType::MeshObject,
            0xbd => PacketType::MeshObjectPlane,
            0xbe => PacketType::StillTextureObject,
            0xbf => PacketType::TextureSpatialLayer,
            0xc0 => PacketType::TextureSnrLayer,
            0xc1 => PacketType::TextureTile,
            0xc2 => PacketType::TextureShapeLayer,
            0xc3 => PacketType::Stuffing,
            sc => PacketType::Unknown(sc),
        };

        Packet {
            ptype,
            offset: offset as u32,
            len: len as u32,
        }
    }

    pub(crate) fn ptype(&self) -> PacketType {
        self.ptype
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset as usize
    }

    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    pub(crate) fn data<'a>(&'a self, frame_data: &'a [u8]) -> &'a [u8] {
        &frame_data[self.offset()..][..self.len()]
    }
}

pub(crate) type PacketVec = SmallVec<[Packet; 8]>;

// Errors that can be produced when parsing
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub(crate) enum Mpeg4ParseError {
    #[error("No packet start codes found")]
    NoSync,

    #[error("Too many packets")]
    TooManyPackets,
}

pub(crate) fn parse_packets_from_slice(frame_data: &[u8]) -> Result<PacketVec, Mpeg4ParseError> {
    // Skip any number of leading zeros
    let Some(first_nonzero) = frame_data.iter().position(|&b| b != 0x00) else {
        return Err(Mpeg4ParseError::NoSync); // all zeros
    };

    // Make sure we have at least two zeroes in front, i.e. 00 00 01
    if first_nonzero < 2 || frame_data[first_nonzero] != 0x01 {
        return Err(Mpeg4ParseError::NoSync);
    }

    let initial_offset = first_nonzero - 2;

    // There are cleverer ways to scan for sync markers, but for now KISS.
    fn scan_for_sync_marker_bit(bytes: &[u8]) -> Option<usize> {
        bytes.windows(3).position(|window| window == [0, 0, 1])
    }

    let mut packets: PacketVec = smallvec::smallvec![];

    let mut frame_data = &frame_data[initial_offset..];
    let mut offset = initial_offset;

    while frame_data.len() > 3 {
        // Look for the start of the next packet to figure out where this packet ends
        let packet = if let Some(next_offset) = scan_for_sync_marker_bit(&frame_data[2..]) {
            let len = next_offset + 2;
            let packet = Packet::new(offset, len, frame_data);
            frame_data = &frame_data[next_offset + 2..];
            packet
        } else {
            // Packet is all the remaining data (we assume parsed input)
            let len = frame_data.len();
            let packet = Packet::new(offset, len, frame_data);
            frame_data = &[];
            packet
        };

        offset += packet.len();

        packets.push(packet);

        // Sanity check
        if packets.len() > 256 {
            return Err(Mpeg4ParseError::TooManyPackets);
        }
    }

    Ok(packets)
}
