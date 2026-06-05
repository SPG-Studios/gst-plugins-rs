// GStreamer RTP MPEG-4 part 2 Video Elementary Stream Depayloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmp4vdepay2
 * @see_also: rtpmp4vpay2, rtpmp4vpay, rtpmp4vdepay, avenc_mpeg4, avdec_mpeg4
 *
 * Depayload an MPEG-4 Video Elementary Stream from RTP packets as per [RFC 3016][rfc-3016].
 *
 * [rfc-3016]: https://www.rfc-editor.org/rfc/rfc3016#section-3
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc address=127.0.0.1 port=5555 caps='application/x-rtp,media=video,clock-rate=90000,encoding-name=MP4V-ES' ! rtpjitterbuffer latency=100 ! rtpmp4vdepay2 ! decodebin3 ! videoconvertscale ! autovideosink
 * ]| This will depayload and decode an incoming RTP MPEG-4 part 2 video stream. You can use the
 * #rtpmp4vpay2 and #avenc_mpeg4 elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::RtpBaseDepay2Ext;

#[derive(Default)]
pub struct RtpMpeg4VideoDepay;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmp4vdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG-4 part 2 Video Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpeg4VideoDepay {
    const NAME: &'static str = "GstRtpMpeg4VideoDepay2";
    type Type = super::RtpMpeg4VideoDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpMpeg4VideoDepay {}

impl GstObjectImpl for RtpMpeg4VideoDepay {}

impl ElementImpl for RtpMpeg4VideoDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG-4 Part 2 Video Elementary Stream Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload an MPEG-4 part 2 Elementary Stream from RTP packets (RFC 3016)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/mpeg")
                    .field("systemstream", false)
                    .field("mpegversion", 4i32)
                    .field("parsed", false)
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        // 90000 is the default clock rate, but others are allowed too
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "MP4V-ES")
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "MP4V-ES")
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

impl crate::basedepay::RtpBaseDepay2Impl for RtpMpeg4VideoDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let mut src_caps = gst::Caps::builder("video/mpeg")
            .field("mpegversion", 4i32)
            .field("systemstream", false)
            .field("parsed", false);

        let s = caps.structure(0).unwrap();

        if let Ok(config_str) = s.get::<&str>("config") {
            let Ok(config_data) = hex::decode(config_str.trim()) else {
                gst::error!(
                    CAT,
                    imp = self,
                    "Could not parse configuration string {config_str}"
                );
                return false;
            };
            let codec_data = gst::Buffer::from_mut_slice(config_data);
            src_caps = src_caps.field("codec_data", codec_data);
        }

        // Can we do something useful with "profile-level-id" here if it's set?
        // Parser will figure it out anyway.

        self.obj().set_src_caps(&src_caps.build());

        true
    }

    // Encapsulation of MPEG Video Elementary Streams:
    // https://datatracker.ietf.org/doc/html/rfc3016#section-3
    //
    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Just push out the payloaded ES data as-is and let someone else do the parsing
        let mut outbuf = packet.payload_buffer();

        // Marker flag indicates end of VOP
        if packet.marker_bit() {
            let outbuf_ref = outbuf.get_mut().unwrap();
            outbuf_ref.set_flags(gst::BufferFlags::MARKER);
        }

        // Note: depayloader base class will set DISCONT on next output buffer if input had one

        gst::trace!(CAT, imp = self, "Finishing buffer {outbuf:?}");

        self.obj().queue_buffer(packet.into(), outbuf)
    }
}

impl RtpMpeg4VideoDepay {}
