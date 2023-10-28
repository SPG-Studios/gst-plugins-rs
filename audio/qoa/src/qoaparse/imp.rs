// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use glib::once_cell::sync::Lazy;
use gst::glib;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use qoaudio::{QOA_HEADER_SIZE, QOA_LMS_LEN, QOA_MAGIC, QOA_MIN_FILESIZE};
use std::fmt;
use std::sync::{Arc, Mutex};

#[derive(Default, Debug, PartialEq)]
struct State {
    last_header: Option<FrameHeader>,
}

#[derive(Default)]
pub struct QoaParse {
    state: Arc<Mutex<State>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "qoaparse",
        gst::DebugColorFlags::empty(),
        Some("Quite OK Audio parser"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for QoaParse {
    const NAME: &'static str = "GstQoaParse";
    type Type = super::QoaParse;
    type ParentType = gst_base::BaseParse;
}

impl ObjectImpl for QoaParse {}

impl GstObjectImpl for QoaParse {}

impl ElementImpl for QoaParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "QOA parser",
                "Codec/Parser/Audio",
                "Quite OK Audio parser",
                "Rafael Caricio <rafael@caricio.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("audio/x-qoa").build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("audio/x-qoa")
                .field("parsed", true)
                .field("rate", gst::IntRange::<i32>::new(1, 16777215))
                .field("channels", &gst::IntRange::<i32>::new(1, 255))
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseParseImpl for QoaParse {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Starting...");

        self.obj().set_min_frame_size(QOA_MIN_FILESIZE as u32);
        Ok(())
    }

    fn handle_frame(
        &self,
        mut frame: gst_base::BaseParseFrame,
    ) -> Result<(gst::FlowSuccess, u32), gst::FlowError> {
        gst::trace!(CAT, imp: self, "Handling frame...");

        let input = frame.buffer().unwrap();
        let map = input.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["Failed to map input buffer readable"]
            );
            gst::FlowError::Error
        })?;
        let data = map.as_slice();

        let file_header_size = {
            if data.len() >= QOA_MIN_FILESIZE {
                let magic = u32::from_be_bytes(data[0..4].try_into().unwrap());
                if magic == QOA_MAGIC {
                    QOA_HEADER_SIZE
                } else {
                    0
                }
            } else {
                0
            }
        };

        // We need at least a full frame header size to proceed
        if data.len() < (file_header_size + QOA_HEADER_SIZE) {
            // Error with not enough bytes to read the frame header
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["Not enough bytes to read the frame header"]
            );
            return Err(gst::FlowError::Error);
        }

        // We want to skip the file header (if content is coming
        // from QOA file) so we ignore the first bytes. We want to
        // read the frame header.
        let raw_header = u64::from_be_bytes(
            data[file_header_size..(file_header_size + QOA_HEADER_SIZE)]
                .try_into()
                .unwrap(),
        );

        // We don't know where in the byte stream we are reading from. We need to figure out where
        // the frame header starts and sync to it. We try to parse and validate some traits of
        // a valid QOA frame header.
        let frame_header = match FrameHeader::parse(raw_header) {
            Ok(header) => header,
            Err(_) => {
                // maybe the next byte will sync with a valid frame header
                return Ok((gst::FlowSuccess::Ok, 1));
            }
        };

        if data.len() < (file_header_size + frame_header.frame_size) {
            gst::trace!(
                CAT,
                imp: self,
                "Not enough bytes to read the frame, need {} bytes, have {}. Waiting for more data...",
                file_header_size + frame_header.frame_size,
                data.len()
            );
            return Ok((gst::FlowSuccess::Ok, 0));
        }

        drop(map);

        let duration = (frame_header.num_samples_per_channel as u64)
            .mul_div_floor(*gst::ClockTime::SECOND, frame_header.sample_rate as u64)
            .map(gst::ClockTime::from_nseconds);

        let buffer = frame.buffer_mut().unwrap();
        buffer.set_duration(duration);
        // all buffers are valid QOA frames which contain header
        buffer.set_flags(gst::BufferFlags::HEADER);

        gst::trace!(
            CAT,
            imp: self,
            "Found frame {frame_header} with duration of {duration:?}",
        );

        let mut state = self.state.lock().unwrap();
        if self.obj().src_pad().current_caps().is_none() || state.last_header != Some(frame_header)
        {
            // Set src pad caps
            let src_caps = gst::Caps::builder("audio/x-qoa")
                .field("parsed", true)
                .field("rate", frame_header.sample_rate as i32)
                .field("channels", frame_header.channels as i32)
                .build();

            gst::debug!(CAT, imp: self, "Setting src pad caps {:?}", src_caps);

            self.obj()
                .src_pad()
                .push_event(gst::event::Caps::new(&src_caps));

            state.last_header = Some(frame_header);
        }

        self.obj()
            .finish_frame(frame, (file_header_size + frame_header.frame_size) as u32)?;

        Ok((gst::FlowSuccess::Ok, 0))
    }
}

pub const MAX_SLICES_PER_CHANNEL_PER_FRAME: usize = 256;

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
    fn parse(frame_header: u64) -> Result<Self, InvalidFrameHeader> {
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
