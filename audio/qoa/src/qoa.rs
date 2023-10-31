use qoaudio::{QOA_HEADER_SIZE, QOA_LMS_LEN};
use std::fmt;

pub const MAX_SLICES_PER_CHANNEL_PER_FRAME: usize = 256;

#[derive(Debug, Copy, Clone)]
pub struct InvalidFrameHeader;

#[derive(Debug, Copy, Clone, Default)]
pub struct FrameHeader {
    /// Number of channels in this frame
    pub channels: u8,
    /// Sample rate in HZ for this frame
    pub sample_rate: u32,
    /// Samples per channel in this frame
    pub num_samples_per_channel: u16,
    /// Total size of the frame (includes header size itself)
    pub frame_size: usize,
}

impl FrameHeader {
    /// Parse and validate various traits of a valid frame header.
    pub fn parse(frame_header: u64) -> Result<Self, InvalidFrameHeader> {
        let channels = ((frame_header >> 56) & 0x0000ff) as u8;
        let sample_rate = ((frame_header >> 32) & 0xffffff) as u32;
        let num_samples_per_channel = ((frame_header >> 16) & 0x00ffff) as u16;
        let frame_size = (frame_header & 0x00ffff) as usize;

        if channels == 0 || sample_rate == 0 {
            return Err(InvalidFrameHeader);
        }

        const LMS_SIZE: usize = 4;
        let non_sample_data_size = QOA_HEADER_SIZE + QOA_LMS_LEN * LMS_SIZE * channels as usize;
        if frame_size <= non_sample_data_size {
            return Err(InvalidFrameHeader);
        }
        let data_size = frame_size - non_sample_data_size;
        let num_slices = data_size / 8;

        if num_slices % channels as usize != 0 {
            return Err(InvalidFrameHeader);
        }
        if num_slices / channels as usize > MAX_SLICES_PER_CHANNEL_PER_FRAME {
            return Err(InvalidFrameHeader);
        }

        Ok(FrameHeader {
            channels,
            sample_rate,
            num_samples_per_channel,
            frame_size,
        })
    }
}

impl PartialEq for FrameHeader {
    fn eq(&self, other: &Self) -> bool {
        self.channels == other.channels && self.sample_rate == other.sample_rate
    }
}

impl fmt::Display for FrameHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{channels={}, sample_rate={}, num_samples_per_channel={}, frame_size={}}}",
            self.channels, self.sample_rate, self.num_samples_per_channel, self.frame_size
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_parse_valid_frame_header() {
        let frame_header = FrameHeader::parse(0x0200a028000003e8).unwrap();
        assert_eq!(
            frame_header,
            FrameHeader {
                channels: 2,
                sample_rate: 41000,
                num_samples_per_channel: 100,
                frame_size: 1000
            }
        );
    }

    #[test]
    fn test_invalid_frame_header() {
        assert!(FrameHeader::parse(0x0000000000000000).is_err());
        assert!(FrameHeader::parse(0x0420420420420420).is_err());
    }
}
