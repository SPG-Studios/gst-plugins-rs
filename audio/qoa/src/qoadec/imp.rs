// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use byte_slice_cast::*;
use gst::glib;
use gst::subclass::prelude::*;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;
use once_cell::sync::Lazy;
use qoaudio::{DecodedAudio, QoaDecoder};

#[derive(Default)]
struct State {
    decoder: Option<QoaDecoder>,
    audio_info: Option<gst_audio::AudioInfo>,
}

#[derive(Default)]
pub struct QoaDec {
    state: AtomicRefCell<Option<State>>,
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

impl ObjectImpl for QoaDec {}

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

        *self.state.borrow_mut() = Some(State::default());
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stopping...");

        *self.state.borrow_mut() = None;
        Ok(())
    }

    fn handle_frame(
        &self,
        inbuf: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp: self, "Handling buffer {:?}", inbuf);

        let inbuf = match inbuf {
            None => return Ok(gst::FlowSuccess::Ok),
            Some(inbuf) => inbuf,
        };

        let inmap = inbuf.map_readable().map_err(|_| {
            gst::error!(CAT, imp: self, "Failed to buffer readable");
            gst::FlowError::Error
        })?;

        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or_else(|| {
            gst::error!(CAT, imp: self, "Failed to get state");
            gst::FlowError::NotNegotiated
        })?;

        self.handle_buffer(state, &inmap)
    }
}

impl QoaDec {
    fn handle_buffer(
        &self,
        state: &mut State,
        indata: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let decoder = match state.decoder {
            Some(ref mut decoder) => decoder,
            None => {
                let decoder = QoaDecoder::decode_header(indata).unwrap_or_else(|err| {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "Using decoder in streaming mode, cause: {}",
                        err
                    );
                    QoaDecoder::streaming()
                });
                state.decoder = Some(decoder);
                state.decoder.as_mut().unwrap()
            }
        };

        gst::trace!(
            CAT,
            imp: self,
            "Trying to decode frame... indata: {}",
            indata.len()
        );

        let audio: DecodedAudio = decoder
            .decode_frames(indata)
            .and_then(|frames| frames.try_into())
            .map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Negotiation,
                    ["Failed to decode frames: {}", err]
                );
                gst::FlowError::Error
            })?;

        gst::trace!(CAT, imp: self, "Decoded audio: {:?}", audio.duration());

        // On new buffers the audio configuration might change, if so we need to request renegotiation
        // and reconfigure the audio info
        if state.audio_info.is_none()
            || state.audio_info.as_ref().unwrap().channels() != audio.channels()
            || state.audio_info.as_ref().unwrap().rate() != audio.sample_rate()
        {
            let audio_info = get_audio_info(&audio).map_err(|e| {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Negotiation,
                    ["Failed to get audio info: {}", e]
                );
                gst::FlowError::Error
            })?;

            gst::debug!(
                CAT,
                imp: self,
                "Successfully parsed headers: {:?}",
                audio_info
            );

            self.obj().set_output_format(&audio_info)?;
            self.obj().negotiate()?;

            state.audio_info = Some(audio_info);
        }

        let samples = audio.collect::<Vec<i16>>();

        struct CastVec(Vec<i16>);
        impl AsRef<[u8]> for CastVec {
            fn as_ref(&self) -> &[u8] {
                self.0.as_byte_slice()
            }
        }
        impl AsMut<[u8]> for CastVec {
            fn as_mut(&mut self) -> &mut [u8] {
                self.0.as_mut_byte_slice()
            }
        }

        let outbuf = gst::Buffer::from_mut_slice(CastVec(samples));
        self.obj().finish_frame(Some(outbuf), 1)
    }
}

fn get_audio_info(audio: &DecodedAudio) -> Result<gst_audio::AudioInfo, String> {
    let index = match audio.channels() as usize {
        0 => return Err("no channels".to_string()),
        n if n > 8 => return Err("more than 8 channels, not supported yet".to_string()),
        n => n,
    };
    let to = &QOA_CHANNEL_POSITIONS[index - 1][..index];
    let info_builder = gst_audio::AudioInfo::builder(
        gst_audio::AUDIO_FORMAT_S16,
        audio.sample_rate(),
        audio.channels(),
    )
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
