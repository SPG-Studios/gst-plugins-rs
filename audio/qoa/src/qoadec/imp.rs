// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::qoa::QOA_MIN_FILESIZE;
use byte_slice_cast::*;
use gst::glib;
use gst::glib::once_cell::sync::Lazy;
use gst::subclass::prelude::*;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;
use qoaudio::{QoaDecoder, QOA_HEADER_SIZE, QOA_MAGIC};
use std::io::Cursor;
use std::sync::{Arc, Mutex};

struct State {
    decoder: QoaDecoder<Cursor<Vec<u8>>>,
    audio_info: Option<gst_audio::AudioInfo>,
}

impl Default for State {
    fn default() -> Self {
        State {
            decoder: QoaDecoder::new_streaming().expect("Decoder creation failed"),
            audio_info: None,
        }
    }
}

#[derive(Default)]
pub struct QoaDec {
    state: Arc<Mutex<Option<State>>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "qoadec",
        gst::DebugColorFlags::empty(),
        Some("Quite OK Audio decoder"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for QoaDec {
    const NAME: &'static str = "GstQoaDec";
    type Type = super::QoaDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for QoaDec {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_drainable(false);
    }
}

impl GstObjectImpl for QoaDec {}

impl ElementImpl for QoaDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "QOA decoder",
                "Decoder/Audio",
                "Quite OK Audio decoder",
                "Rafael Caricio <rafael@caricio.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("audio/x-qoa")
                .field("parsed", true)
                .field("rate", gst::IntRange::<i32>::new(1, 16777215))
                .field("channels", gst::IntRange::<i32>::new(1, 255))
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format(gst_audio::AUDIO_FORMAT_S16)
                .rate_range(1..16_777_215)
                .channels_range(1..8)
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AudioDecoderImpl for QoaDec {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Starting...");

        let mut state = self.state.lock().unwrap();
        *state = Some(State::default());
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stopping...");

        let mut state = self.state.lock().unwrap();
        *state = None;

        Ok(())
    }

    fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Setting format {:?}", caps);

        let s = caps.structure(0).unwrap();
        let channels = s.get::<i32>("channels").unwrap();
        let rate = s.get::<i32>("rate").unwrap();

        if let Ok(audio_info) = get_audio_info(rate as u32, channels as u32) {
            if self.obj().set_output_format(&audio_info).is_err() || self.obj().negotiate().is_err()
            {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Error to negotiate output from based on in-caps parameters"
                );
            }

            let mut state_guard = self.state.lock().unwrap();
            let state = state_guard.as_mut().unwrap();
            state.audio_info = Some(audio_info);
        } else {
            gst::debug!(
                CAT,
                imp: self,
                "Failed to get audio info"
            );
        }

        Ok(())
    }

    fn handle_frame(
        &self,
        inbuf: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp: self, "Handling buffer {:?}", inbuf);

        let inbuf = inbuf.expect("Non-drainable should never receive empty buffer");
        let inmap = inbuf.map_readable().map_err(|_| {
            gst::error!(CAT, imp: self, "Failed to buffer readable");
            gst::FlowError::Error
        })?;

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or_else(|| {
            gst::error!(CAT, imp: self, "Failed to get state");
            gst::FlowError::NotNegotiated
        })?;

        // Skip file header, if present
        let file_header_size = {
            if inmap.len() >= QOA_MIN_FILESIZE {
                let magic = u32::from_be_bytes(inmap[0..4].try_into().unwrap());
                if magic == QOA_MAGIC {
                    QOA_HEADER_SIZE
                } else {
                    0
                }
            } else {
                0
            }
        };

        let samples = state
            .decoder
            .decode_frame(&inmap[file_header_size..])
            .map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Negotiation,
                    ["Failed to decode frames: {}", err]
                );
                gst::FlowError::Error
            })?;

        gst::trace!(CAT, imp: self, "Successfully decoded {} audio samples from frame", samples.len());

        let outbuf = gst::Buffer::from_slice(samples.as_byte_slice().to_vec());
        self.obj().finish_frame(Some(outbuf), 1)
    }
}

fn get_audio_info(sample_rate: u32, channels: u32) -> Result<gst_audio::AudioInfo, String> {
    let index = match channels as usize {
        0 => return Err("no channels".to_string()),
        n if n > 8 => return Err("more than 8 channels, not supported yet".to_string()),
        n => n,
    };
    let to = &QOA_CHANNEL_POSITIONS[index - 1][..index];
    let info_builder =
        gst_audio::AudioInfo::builder(gst_audio::AUDIO_FORMAT_S16, sample_rate, channels)
            .positions(to);

    let audio_info = info_builder
        .build()
        .map_err(|e| format!("failed to build audio info: {e}"))?;

    Ok(audio_info)
}

const QOA_CHANNEL_POSITIONS: [[gst_audio::AudioChannelPosition; 8]; 8] = [
    [
        gst_audio::AudioChannelPosition::Mono,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearCenter,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
    ],
];
