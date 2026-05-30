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

use symphonia::core::codecs::audio::AudioCodecParameters;
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
use symphonia::core::io::{BufReader, ReadBytes};
use symphonia::default::codecs::FlacDecoder;

use std::sync::LazyLock;

use crate::common;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "symphoniaflacdec",
        gst::DebugColorFlags::empty(),
        Some("Symphonia FLAC decoder"),
    )
});

struct Context {
    decoder: FlacDecoder,
    out_audio_info: Option<gst_audio::AudioInfo>,
}

impl Context {
    fn new(codec_params: AudioCodecParameters, out_audio_info: gst_audio::AudioInfo) -> Self {
        use symphonia::core::codecs::audio::AudioDecoderOptions;

        let decoder = match FlacDecoder::try_new(
            &codec_params,
            // FIXME make gapless option setting dependent
            &AudioDecoderOptions::default(),
        ) {
            Ok(decoder) => decoder,
            Err(err) => panic!("Failed to build decoder: {err}"),
        };

        Context {
            decoder,
            out_audio_info: Some(out_audio_info),
        }
    }

    fn reset(&mut self) {
        self.decoder.reset();
        self.out_audio_info = None;
    }
}

#[derive(Default)]
pub struct SymphoniaFlacDec {
    context: AtomicRefCell<Option<Context>>,
}

#[glib::object_subclass]
impl ObjectSubclass for SymphoniaFlacDec {
    const NAME: &'static str = "SymphoniaFlacDec";
    type Type = super::SymphoniaFlacDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for SymphoniaFlacDec {}

impl GstObjectImpl for SymphoniaFlacDec {}

impl ElementImpl for SymphoniaFlacDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Symphonia FLAC decoder",
                "Codec/Decoder/Audio",
                "Symphonia FLAC (Free Lossless Audio Codec) decoder",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-flac")
                .field("framed", true)
                .rate_range(1i32..655_350)
                .channels_range(1i32..8)
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

impl AudioDecoderImpl for SymphoniaFlacDec {
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping");
        *self.context.borrow_mut() = None;
        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        Ok(())
    }

    fn flush(&self, _hard: bool) {
        gst::debug!(CAT, imp = self, "Flushing");

        let mut context_guard = self.context.borrow_mut();
        if let Some(ref mut context) = *context_guard {
            context.reset();
        }
    }

    fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Attempting to set format");
        gst::log!(CAT, imp = self, "sink {caps:?}");

        let s = caps.structure(0).unwrap();
        if let Ok(Some(streamheaders)) = s.get_optional::<gst::ArrayRef>("streamheader") {
            if streamheaders.len() < 2 {
                gst::debug!(CAT, imp = self, "Not enough streamheaders, trying in-band");
                return Ok(());
            }

            // Stream info block is expected to be the first
            if let Ok(Some(header_buf)) = streamheaders[0].get::<Option<gst::Buffer>>() {
                let headermap = header_buf.map_readable().unwrap();
                let mut reader = BufReader::new(headermap.as_slice());

                let mut marker = [0u8; 5];
                if reader.read_buf_exact(&mut marker).is_err() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Invalid streamheader len, will try in-band"
                    );
                    return Ok(());
                }

                if marker != *b"\x7FFLAC" {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Invalid streamheader format, will try in-band"
                    );
                    return Ok(());
                }

                // Skip: version (2) + num headers (2) + 'fLaC' (4)
                if reader.ignore_bytes(2 + 2 + 4).is_err() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Invalid streamheader version block, will try in-band"
                    );
                    return Ok(());
                }

                let codec_params = match Self::try_parse_stream_info(&mut reader) {
                    Ok(Some(codec_params)) => codec_params,
                    Ok(None) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Couldn't find Stream Info in first header, will try in-band",
                        );
                        return Ok(());
                    }
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Couldn't get stream info from headers, will try in-band",
                        );
                        gst::debug!(CAT, imp = self, "due to: {err}");
                        return Ok(());
                    }
                };

                let mut context = self.context.borrow_mut();
                self.negotiate_src_format(&mut context, codec_params)
                    .map_err(|err| {
                        gst::loggable_error!(
                            CAT,
                            "Failed to negotiate src caps from the stream info: {err}",
                        )
                    })?;
            }
        }

        Ok(())
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

        if context_guard.is_none() {
            // Not ready yet, expecting the Stream Info frame
            let mut reader = BufReader::new(inmap.as_slice());
            let codec_params = Self::try_parse_stream_info(&mut reader).map_err(|err| {
                gst::error!(CAT, imp = self, "Failed to parse Stream Info: {err}");
                gst::FlowError::NotNegotiated
            })?;
            let Some(codec_params) = codec_params else {
                gst::debug!(CAT, imp = self, "Skipping x{:02x?}", inmap[0]);
                return self.obj().finish_frame(None, 1);
            };

            self.negotiate_src_format(&mut context_guard, codec_params)
                .map_err(|err| {
                    gst::error!(CAT, imp = self, "Failed to negotiate src pad caps: {err}");
                    gst::FlowError::NotNegotiated
                })?;
        }

        let context = context_guard
            .as_mut()
            .ok_or(gst::FlowError::NotNegotiated)?;

        if inmap[0] != 0b1111_1111 || inmap[1] & 0b1111_1100 != 0b1111_1000 {
            // info about other headers in flacparse and https://xiph.org/flac/format.html
            gst::debug!(CAT, imp = self, "Skipping header buffer x{:02x?}", inmap[0]);
            return self.obj().finish_frame(None, 1);
        }

        gst::log!(CAT, imp = self, "Data buffer received");

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

        let out_audio_info = context.out_audio_info.as_ref().unwrap();
        let obj = self.obj();
        // FIXME handle channels mapping
        let outbuf = common::to_outbuf(
            obj.upcast_ref::<gst_audio::AudioDecoder>(),
            decoded,
            out_audio_info,
        )?;

        obj.finish_frame(Some(outbuf), 1)
    }
}

#[derive(Debug, thiserror::Error)]
enum ParseStreamInfoError {
    #[error("Wrong Stream Info size: {}", 0)]
    WrongSize(u64),
    #[error("Invalid Stream Info: {}", 0)]
    Invalid(String),
    #[error("Header is not Stream Info and was the last one")]
    NoMoreHeaders,
}

impl SymphoniaFlacDec {
    fn try_parse_stream_info(
        reader: &mut BufReader,
    ) -> Result<Option<AudioCodecParameters>, ParseStreamInfoError> {
        use symphonia::core::codecs::audio::VerificationCheck;
        use symphonia::core::io::{FiniteStream, ScopedStream};

        use symphonia_common::xiph::audio::flac::{
            MetadataBlockHeader, MetadataBlockType, StreamInfo,
        };

        use ParseStreamInfoError::*;

        let Ok(header) = MetadataBlockHeader::read(reader) else {
            // Most likely not a header
            return Ok(None);
        };

        let mut block_stream = ScopedStream::new(reader, header.block_len.into());

        if let MetadataBlockType::StreamInfo = header.block_type {
            if !StreamInfo::is_valid_size(block_stream.byte_len()) {
                return Err(WrongSize(block_stream.byte_len()));
            }

            let extra_data = block_stream
                .read_boxed_slice_exact(block_stream.byte_len() as usize)
                .unwrap();

            let info = StreamInfo::read(&mut BufReader::new(&extra_data))
                .map_err(|err| Invalid(err.to_string()))?;

            let mut codec_params = AudioCodecParameters::new();
            codec_params
                .for_codec(CODEC_ID_FLAC)
                .with_extra_data(extra_data)
                .with_sample_rate(info.sample_rate)
                .with_bits_per_sample(info.bits_per_sample)
                .with_channels(info.channels);

            if let Some(md5) = info.md5 {
                codec_params.with_verification_code(VerificationCheck::Md5(md5));
            }

            Ok(Some(codec_params))
        } else {
            // Not the Stream Info header
            if header.is_last {
                // ... and there won't be any other chance to get one
                return Err(NoMoreHeaders);
            }

            Ok(None)
        }
    }

    fn negotiate_src_format(
        &self,
        context: &mut Option<Context>,
        codec_params: AudioCodecParameters,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(
            CAT,
            imp = self,
            "Negotiating src format for {codec_params:?}",
        );

        // Negotiate output audio format and layout
        let src_pad = self.obj().static_pad("src").unwrap();
        let filter = caps_filter_from_params(&codec_params).map_err(|err| {
            gst::error!(CAT, imp = self, "Couldn't build caps filter: {err}");
            gst::FlowError::NotNegotiated
        })?;

        gst::debug!(CAT, imp = self, "Proposing {:?}", filter);
        let mut caps = src_pad.peer_query_caps(Some(&filter));
        gst::debug!(CAT, imp = self, "Peer refined caps to {caps:?}");

        caps.fixate();
        if caps.is_empty() {
            src_pad.mark_reconfigure();
            gst::error!(CAT, imp = self, "Failed to negotiate src pad {caps:?}");
            return Err(gst::FlowError::NotNegotiated);
        }

        let audio_info = gst_audio::AudioInfo::from_caps(&caps)
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
        *context = Some(Context::new(codec_params, audio_info));

        Ok(gst::FlowSuccess::Ok)
    }
}

#[derive(Debug, thiserror::Error)]
enum CapsFilterError {
    #[error("invalid sample rate")]
    InvalidSampleRate,
    #[error("more than 8 channels, not supported yet")]
    MoreThan8Channels,
    #[error("no channels")]
    NoChannels,
    #[error("unsupported bits per sample format: {}", 0)]
    UnsupportedBitsPerSample(u32),
}

/// Builds caps suitable to initiate decoder downstream negotiation.
fn caps_filter_from_params(params: &AudioCodecParameters) -> Result<gst::Caps, CapsFilterError> {
    use CapsFilterError::*;

    let in_format = match params.bits_per_sample.unwrap() {
        8 => gst_audio::AUDIO_FORMAT_S8,
        16 => gst_audio::AUDIO_FORMAT_S16,
        24 => gst_audio::AUDIO_FORMAT_S24,
        32 => gst_audio::AUDIO_FORMAT_F32,
        other => return Err(UnsupportedBitsPerSample(other)),
    };

    let sample_rate = params
        .sample_rate
        .filter(|&rate| rate > 0)
        .ok_or(InvalidSampleRate)?;

    let channels = match params.channels.as_ref().map_or(0, |chans| chans.count()) {
        0 => return Err(NoChannels),
        n if n > 8 => return Err(MoreThan8Channels),
        n => n,
    };
    let positions = &FLAC_CHANNEL_POSITIONS[channels - 1][..channels];

    Ok(common::build_caps_filter(
        gst_audio::AudioInfo::builder(in_format, sample_rate, channels as u32)
            .positions(positions)
            .build()
            .unwrap(),
    ))
}

// http://www.xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-800004.3.9
// http://flac.sourceforge.net/format.html#frame_header
const FLAC_CHANNEL_POSITIONS: [[gst_audio::AudioChannelPosition; 8]; 8] = [
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
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
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
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    // FIXME: 7/8 channel layouts are not defined in the FLAC specs
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
    ],
];
