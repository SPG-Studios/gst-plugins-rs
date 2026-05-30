// Copyright (C) 2022-2026 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::glib;
use gst::subclass::prelude::*;
use gst_audio::audio_decoder_error;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia::default::codecs::MpaDecoder;

use std::sync::LazyLock;

use crate::common;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "symphoniamp3dec",
        gst::DebugColorFlags::empty(),
        Some("Symphonia MP3 decoder"),
    )
});

struct Context {
    decoder: MpaDecoder,
    out_audio_info: Option<gst_audio::AudioInfo>,
}

impl Default for Context {
    fn default() -> Self {
        use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};

        let decoder = match MpaDecoder::try_new(
            AudioCodecParameters::new().for_codec(CODEC_ID_MP3),
            // FIXME make gapless option setting dependent
            &AudioDecoderOptions::default(),
        ) {
            Ok(decoder) => decoder,
            Err(err) => panic!("Failed to build decoder: {err}"),
        };

        Context {
            decoder,
            out_audio_info: None,
        }
    }
}

impl Context {
    fn reset(&mut self) {
        self.decoder.reset();
        self.out_audio_info = None;
    }
}

#[derive(Default)]
pub struct SymphoniaMp3Dec {
    context: AtomicRefCell<Option<Context>>,
}

#[glib::object_subclass]
impl ObjectSubclass for SymphoniaMp3Dec {
    const NAME: &'static str = "SymphoniaMp3Dec";
    type Type = super::SymphoniaMp3Dec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for SymphoniaMp3Dec {}

impl GstObjectImpl for SymphoniaMp3Dec {}

impl ElementImpl for SymphoniaMp3Dec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Symphonia MP3 decoder",
                "Codec/Decoder/Audio",
                "Symphonia MP3 (MPEG audio layer 3) decoder",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_audio::AudioCapsBuilder::for_encoding("audio/mpeg")
                .field("mpegversion", 1i32)
                .field("layer", gst::IntRange::new(1i32, 3))
                .field("parsed", true)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![sink_pad_template, common::src_caps_template()]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AudioDecoderImpl for SymphoniaMp3Dec {
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping");
        *self.context.borrow_mut() = None;
        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        *self.context.borrow_mut() = Some(Context::default());
        Ok(())
    }

    fn flush(&self, _hard: bool) {
        gst::debug!(CAT, imp = self, "Flushing");

        let mut context_guard = self.context.borrow_mut();
        if let Some(ref mut context) = *context_guard {
            context.reset();
        }
    }

    fn handle_frame(
        &self,
        inbuf: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use symphonia::core::packet::PacketRef;
        use symphonia::core::units::{Duration, Timestamp};

        let Some(inbuf) = inbuf else {
            return Ok(gst::FlowSuccess::Ok);
        };

        gst::log!(CAT, imp = self, "Handling buffer {inbuf:?}");

        let inmap = inbuf.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map buffer readable");
            gst::FlowError::Error
        })?;

        // Ignore empty packets
        if inmap.is_empty() {
            return self.obj().finish_frame(None, 1);
        }

        let mut context_guard = self.context.borrow_mut();

        let context = context_guard
            .as_mut()
            .ok_or(gst::FlowError::NotNegotiated)?;

        // FIXME timestamp / duration for gapless?
        let packet = PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, inmap.as_slice());

        use symphonia::core::errors::Error::*;
        let decoded = match context.decoder.decode_ref(&packet) {
            Ok(decoded) => decoded,
            Err(DecodeError(err)) => {
                gst::warning!(CAT, imp = self, "an error occured decoding a packet: {err}");
                return self.obj().finish_frame(None, 1);
            }
            Err(ResetRequired) => {
                gst::info!(CAT, imp = self, "stream params changed");
                context.reset();
                // FIXME is this the right way to handle this?
                // or is it good enough to call Pad::mark_reconfigure?
                let obj = self.obj();
                obj.static_pad("src")
                    .unwrap()
                    .push_event(gst::event::Reconfigure::new());
                return obj.finish_frame(None, 1);
            }
            Err(err) => {
                return audio_decoder_error!(
                    self.obj(),
                    1,
                    gst::StreamError::Decode,
                    ["an unrecoverable error occured decoding a packet: {err}"]
                );
            }
        };

        if context.out_audio_info.is_none() {
            // Negotiate output audio format and layout
            let src_pad = self.obj().static_pad("src").unwrap();
            let filter = caps_filter_from_decoded(&decoded).map_err(|err| {
                gst::error!(CAT, imp = self, "Couldn't build caps filter: {err}");
                gst::FlowError::NotNegotiated
            })?;

            gst::debug!(CAT, imp = self, "Proposing {filter:?}");
            let mut caps = src_pad.peer_query_caps(Some(&filter));
            gst::debug!(CAT, imp = self, "Peer refined caps to {caps:?}");

            caps.fixate();
            if caps.is_empty() {
                src_pad.mark_reconfigure();
                gst::error!(CAT, imp = self, "Failed to negotiate src pad caps {caps:?}");
                return Err(gst::FlowError::NotNegotiated);
            }

            let out_audio_info = gst_audio::AudioInfo::from_caps(&caps)
                .ok()
                .and_then(|audio_info| {
                    self.obj()
                        .set_output_format(&audio_info)
                        .ok()
                        .map(|_| audio_info)
                })
                .ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Failed to set output format");
                    gst::FlowError::NotSupported
                })?;

            gst::info!(CAT, imp = self, "Using {caps:?}");
            context.out_audio_info = Some(out_audio_info);
        }

        let out_audio_info = context.out_audio_info.as_ref().unwrap();
        let obj = self.obj();
        let outbuf = common::to_outbuf(
            obj.upcast_ref::<gst_audio::AudioDecoder>(),
            decoded,
            out_audio_info,
        )?;

        obj.finish_frame(Some(outbuf), 1)
    }
}

#[derive(Debug, thiserror::Error)]
enum CapsFilterError {
    #[error("invalid sample rate")]
    InvalidSampleRate,
    #[error("more than 2 channels, not supported yet")]
    MoreThan2Channels,
    #[error("no channels")]
    NoChannels,
}

/// Builds caps suitable to initiate decoder downstream negotiation.
fn caps_filter_from_decoded(decoded: &GenericAudioBufferRef) -> Result<gst::Caps, CapsFilterError> {
    use CapsFilterError::*;
    use symphonia::core::audio::GenericAudioBufferRef::*;

    let in_format = match decoded {
        U8(_) => gst_audio::AUDIO_FORMAT_U8,
        U16(_) => gst_audio::AUDIO_FORMAT_U16,
        U24(_) => gst_audio::AUDIO_FORMAT_U24,
        U32(_) => gst_audio::AUDIO_FORMAT_U32,
        S8(_) => gst_audio::AUDIO_FORMAT_S8,
        S16(_) => gst_audio::AUDIO_FORMAT_S16,
        S32(_) => gst_audio::AUDIO_FORMAT_S32,
        S24(_) => gst_audio::AUDIO_FORMAT_S24,
        F32(_) => gst_audio::AUDIO_FORMAT_F32,
        F64(_) => gst_audio::AUDIO_FORMAT_F64,
    };

    let sample_rate = decoded.spec().rate();
    if sample_rate == 0 {
        return Err(InvalidSampleRate);
    }

    let channels = match decoded.spec().channels().count() {
        0 => return Err(NoChannels),
        n if n > 2 => return Err(MoreThan2Channels),
        n => n,
    };

    Ok(common::build_caps_filter(
        gst_audio::AudioInfo::builder(in_format, sample_rate, channels as u32)
            .build()
            .unwrap(),
    ))
}
