// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use qoaudio::{QOA_HEADER_SIZE, QOA_MAGIC, QOA_MIN_FILESIZE};

#[derive(Default)]
pub struct QoaParse;

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

        if self.obj().src_pad().current_caps().is_none() {
            // Set src pad caps
            let src_caps = gst::Caps::builder("audio/x-qoa")
                .field("parsed", true)
                .build();

            gst::debug!(CAT, imp: self, "Setting src pad caps {:?}", src_caps);

            self.obj()
                .src_pad()
                .push_event(gst::event::Caps::new(&src_caps));
        }

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

        if data.len() < (file_header_size + QOA_HEADER_SIZE) {
            // Error with not enough bytes to read the frame header
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["Not enough bytes to read the frame header"]
            );
            return Err(gst::FlowError::Error);
        }

        let frame_header = u64::from_be_bytes(
            data[file_header_size..(file_header_size + QOA_HEADER_SIZE)]
                .try_into()
                .unwrap(),
        );
        let channels = ((frame_header >> 56) & 0x0000ff) as u64;
        let sample_rate = ((frame_header >> 32) & 0xffffff) as u64;
        let total_samples = ((frame_header >> 16) & 0x00ffff) as u64;
        let frame_size = (frame_header & 0x00ffff) as usize;

        if data.len() < (file_header_size + frame_size) {
            gst::trace!(
                CAT,
                imp: self,
                "Not enough bytes to read the frame, need {} bytes, have {}. Waiting for more data...",
                file_header_size + frame_size,
                data.len()
            );
            return Ok((gst::FlowSuccess::Ok, 0));
        }

        drop(map);

        let duration = total_samples
            .mul_div_floor(*gst::ClockTime::SECOND, sample_rate)
            .map(gst::ClockTime::from_nseconds);

        let buffer = frame.buffer_mut().unwrap();
        buffer.set_duration(duration);
        if file_header_size > 0 {
            buffer.set_flags(gst::BufferFlags::HEADER);
        }

        gst::trace!(
            CAT,
            imp: self,
            "Found frame channels={channels}, sample_rate={sample_rate}, total_samples={total_samples}, size={frame_size}, duration={duration:?}",
        );

        self.obj()
            .finish_frame(frame, (file_header_size + frame_size) as u32)?;

        Ok((gst::FlowSuccess::Ok, 0))
    }
}
