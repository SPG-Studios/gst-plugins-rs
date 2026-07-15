// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpjxsvpay
 * @see_also: rtpjxsvdepay, svtjpegxsenc, svtjpegxsdec
 *
 * Payload a JPEG XS video stream into RTP packets as per [RFC 9134][rfc-9134].
 *
 * [rfc-9134]: https://www.rfc-editor.org/rfc/rfc9134.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 ... ! svtjpegxsenc ! rtpjxsvpay ! udpsink host=127.0.0.1 port=5004
 * ]| Payload a bare JPEG XS codestream from `svtjpegxsenc`, wrapping each encoded
 * frame into an RFC 9134 picture segment before RTP packetization.
 *
 * A pre-boxed `video/x-jxsv` picture segment (from an upstream element that already
 * emits RFC 9134 picture segments) may also be passed through unchanged.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;
use glib::ParamSpecBuilderExt;
use gst::{glib, prelude::*, subclass::prelude::*};
use std::cmp;
use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::LazyLock;

use crate::{
    basepay::{RtpBasePay2Ext, RtpBasePay2ImplExt},
    jxsv::{
        payload_header::{PayloadHeader, format_exact_framerate},
        picture_segment::{PictureSegmentBuilder, is_picture_segment, profile_level_warnings},
        profile_level::ProfileLevel,
    },
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpjxsvpay",
        gst::DebugColorFlags::empty(),
        Some("RTP JPEG XS Payloader"),
    )
});

#[derive(Default)]
struct State {
    frame_counter: u8,
    input_is_picture_segment: bool,
    segment_builder: Option<PictureSegmentBuilder>,
    picture_segment: Vec<u8>,
    profile_level: ProfileLevel,
    validated_first_buffer: bool,
}

const DEFAULT_MAX_CODESTREAM_BITRATE: u64 = 0;

#[derive(Default)]
pub struct RtpJxsvPay {
    state: AtomicRefCell<State>,
    max_codestream_bitrate: AtomicU64,
}

#[glib::object_subclass]
impl ObjectSubclass for RtpJxsvPay {
    const NAME: &'static str = "GstRtpJxsvPay";
    type Type = super::RtpJxsvPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpJxsvPay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> =
            LazyLock::new(|| {
                vec![glib::ParamSpecUInt64::builder("max-codestream-bitrate")
                .nick("Maximum codestream bit rate")
                .blurb(
                    "Maximum JPEG XS codestream bit rate in bits per second for the jpvi brat \
                     field. 0 estimates from codestream size and frame rate.",
                )
                .default_value(DEFAULT_MAX_CODESTREAM_BITRATE)
                .mutable_ready()
                .build()]
            });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "max-codestream-bitrate" => self.max_codestream_bitrate.store(
                value.get().expect("type checked upstream"),
                Ordering::Relaxed,
            ),
            name => unimplemented!("Property '{name}'"),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-codestream-bitrate" => self
                .max_codestream_bitrate
                .load(Ordering::Relaxed)
                .to_value(),
            name => unimplemented!("Property '{name}'"),
        }
    }
}

impl GstObjectImpl for RtpJxsvPay {}

impl ElementImpl for RtpJxsvPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP JPEG XS payloader",
                "Codec/Payloader/Network/RTP",
                "Payload a JPEG XS video stream to RTP packets (RFC 9134)",
                "Gareth Sylvester-Bradley <garethsb@nvidia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = crate::jxsv::media_pad_caps();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("payload", 96i32)
                            .field("clock-rate", 90_000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "jxsv")
                            .field("clock-rate", 90_000i32)
                            .field("packetmode", 0i32)
                            .field("transmode", 1i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpJxsvPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        gst::debug!(CAT, imp = self, "received caps {caps:?}");

        let s = caps.structure(0).unwrap();
        let media_type = s.name();
        let input_is_picture_segment = media_type == crate::jxsv::MEDIA_TYPE_JXSV;

        let interlace_mode = s.get::<&str>("interlace-mode").unwrap_or("progressive");
        if interlace_mode != "progressive" {
            gst::error!(
                CAT,
                imp = self,
                "Only progressive JPEG XS is supported, got interlace-mode={interlace_mode}"
            );
            return false;
        }

        let profile_level = match ProfileLevel::from_caps(s) {
            Ok(profile_level) => profile_level,
            Err(err) => {
                gst::error!(CAT, imp = self, "{err}");
                return false;
            }
        };

        let segment_builder = if input_is_picture_segment {
            None
        } else {
            let max_codestream_bitrate = self.max_codestream_bitrate.load(Ordering::Relaxed);
            match PictureSegmentBuilder::from_caps(caps, max_codestream_bitrate) {
                Ok(builder) => Some(builder),
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to build JPEG XS picture segment template: {err}"
                    );
                    return false;
                }
            }
        };

        let mut caps_builder = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90_000i32)
            .field("encoding-name", "jxsv")
            .field("packetmode", 0i32)
            .field("transmode", 1i32);

        // width, height and depth are integers on the JPEG XS caps but are
        // carried as SDP fmtp parameters, i.e. as strings, on the RTP caps.
        if let Ok(width) = s.get::<i32>("width") {
            caps_builder = caps_builder.field("width", width.to_string());
        }
        if let Ok(height) = s.get::<i32>("height") {
            caps_builder = caps_builder.field("height", height.to_string());
        }
        if let Ok(depth) = s.get::<i32>("depth") {
            caps_builder = caps_builder.field("depth", depth.to_string());
        }
        if let Ok(sampling) = s.get::<&str>("sampling") {
            caps_builder = caps_builder.field("sampling", sampling);
        }
        if let Some(framerate) = s
            .get::<gst::Fraction>("framerate")
            .ok()
            .filter(|fps| *fps > gst::Fraction::new(0, 1))
        {
            caps_builder = caps_builder.field("exactframerate", format_exact_framerate(framerate));
        }
        caps_builder = profile_level.add_to_caps(caps_builder);

        self.obj().set_src_caps(&caps_builder.build());

        let mut state = self.state.borrow_mut();
        state.input_is_picture_segment = input_is_picture_segment;
        state.segment_builder = segment_builder;
        state.picture_segment.clear();
        state.profile_level = profile_level;
        state.validated_first_buffer = false;

        true
    }

    fn negotiate(&self, mut src_caps: gst::Caps) {
        src_caps.truncate();
        {
            let src_caps = src_caps.get_mut().unwrap();
            let s = src_caps.structure_mut(0).unwrap();
            s.fixate_field_str("encoding-name", "jxsv");
        }

        self.parent_negotiate(src_caps);
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let max_payload_size = self.obj().max_payload_size();

        gst::trace!(CAT, imp = self, "received buffer of size {}", buffer.size());

        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        if state.input_is_picture_segment {
            if !is_picture_segment(map.as_ref()) {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["Expected RFC 9134 picture segment starting with a jpvs box"]
                );
                return Err(gst::FlowError::Error);
            }
            if !state.validated_first_buffer {
                for warning in profile_level_warnings(map.as_ref(), &state.profile_level) {
                    gst::warning!(CAT, imp = self, "{warning}");
                }
                state.validated_first_buffer = true;
            }
            state.picture_segment.clear();
            state.picture_segment.extend_from_slice(map.as_ref());
        } else {
            let builder = state.segment_builder.as_ref().ok_or_else(|| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["JPEG XS picture segment builder not configured"]
                );
                gst::FlowError::Error
            })?;

            state.picture_segment = builder
                .wrap_codestream(map.as_ref(), buffer.pts())
                .map(|(picture_segment, warnings)| {
                    if !state.validated_first_buffer {
                        for warning in warnings {
                            gst::warning!(CAT, imp = self, "{warning}");
                        }
                        state.validated_first_buffer = true;
                    }
                    picture_segment
                })
                .map_err(|err| {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["Failed to build JPEG XS picture segment: {err}"]
                    );
                    gst::FlowError::Error
                })?;
        }

        let frame_counter = state.frame_counter;
        let max_data_per_packet = max_payload_size
            .checked_sub(PayloadHeader::SIZE as u32)
            .ok_or_else(|| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["Too small MTU configured for stream"]
                );
                gst::FlowError::Error
            })?;

        let mut data = state.picture_segment.as_slice();
        let mut packet_index = 0u32;

        while !data.is_empty() {
            let payload_size = cmp::min(data.len(), max_data_per_packet as usize);
            let last = data.len() == payload_size;
            let header = PayloadHeader::progressive_codestream(frame_counter, packet_index, last);
            let header_bytes = header.pack();

            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .marker_bit(last)
                    .payload(header_bytes.as_slice())
                    .payload(&data[..payload_size]),
            )?;

            data = &data[payload_size..];
            packet_index += 1;
        }

        state.frame_counter = state.frame_counter.wrapping_add(1) & 0x1f;

        Ok(gst::FlowSuccess::Ok)
    }
}
