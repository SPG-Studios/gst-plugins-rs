// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use anyhow::{Context as _, bail};

/// RFC 9134 JPEG XS RTP payload header (4 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadHeader {
    pub transmission_mode: bool,
    pub packetization_mode: bool,
    pub last: bool,
    pub interlaced: u8,
    pub frame_counter: u8,
    pub sep_counter: u16,
    pub p_counter: u16,
}

impl PayloadHeader {
    pub const SIZE: usize = 4;

    pub fn pack(&self) -> [u8; Self::SIZE] {
        let mut val = 0u32;
        if self.transmission_mode {
            val |= 1 << 31;
        }
        if self.packetization_mode {
            val |= 1 << 30;
        }
        if self.last {
            val |= 1 << 29;
        }
        val |= (u32::from(self.interlaced & 0x3)) << 27;
        val |= (u32::from(self.frame_counter & 0x1f)) << 22;
        val |= (u32::from(self.sep_counter & 0x7ff)) << 11;
        val |= u32::from(self.p_counter & 0x7ff);
        val.to_be_bytes()
    }

    pub fn parse(data: &[u8]) -> Result<Self, anyhow::Error> {
        if data.len() < Self::SIZE {
            bail!("JXSV payload header too short");
        }

        let val = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        Ok(Self {
            transmission_mode: val & (1 << 31) != 0,
            packetization_mode: val & (1 << 30) != 0,
            last: val & (1 << 29) != 0,
            interlaced: ((val >> 27) & 0x3) as u8,
            frame_counter: ((val >> 22) & 0x1f) as u8,
            sep_counter: ((val >> 11) & 0x7ff) as u16,
            p_counter: (val & 0x7ff) as u16,
        })
    }

    pub fn progressive_codestream(frame_counter: u8, packet_index: u32, last: bool) -> Self {
        let p_counter = (packet_index % 2048) as u16;
        let sep_counter = (packet_index / 2048) as u16;
        Self {
            transmission_mode: true,
            packetization_mode: false,
            last,
            interlaced: 0,
            frame_counter,
            sep_counter,
            p_counter,
        }
    }
}

pub fn format_exact_framerate(framerate: gst::Fraction) -> String {
    if framerate.denom() == 1 {
        framerate.numer().to_string()
    } else {
        format!("{}/{}", framerate.numer(), framerate.denom())
    }
}

pub fn parse_exact_framerate(s: &str) -> Result<gst::Fraction, anyhow::Error> {
    let s = s.trim();
    if let Some((numer, denom)) = s.split_once('/') {
        let numer = numer
            .trim()
            .parse::<i32>()
            .context("exactframerate numerator")?;
        let denom = denom
            .trim()
            .parse::<i32>()
            .context("exactframerate denominator")?;
        Ok(gst::Fraction::new(numer, denom))
    } else {
        let numer = s.parse::<i32>().context("exactframerate")?;
        Ok(gst::Fraction::new(numer, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_progressive_codestream_header() {
        let header = PayloadHeader::progressive_codestream(7, 2049, true);
        let packed = header.pack();
        let parsed = PayloadHeader::parse(&packed).unwrap();
        assert_eq!(header, parsed);
        assert_eq!(header.p_counter, 1);
        assert_eq!(header.sep_counter, 1);
    }
}
