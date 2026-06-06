// GStreamer RTP MPEG-4 part 2 Video Elementary Stream Payloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmp4vpay2
 * @see_also: rtpmp4vdepay2, rtpmp4vdepay, rtpmp4vpay, avdec_mpeg4, avenc_mpeg4
 *
 * Payload an MPEG-4 part 2 Video Elementary Stream into RTP packets as per [RFC 3016][rfc-3016].
 *
 * [rfc-3016]: https://datatracker.ietf.org/doc/html/rfc3016#section-3
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,width=1280,height=720,format=I420 ! timeoverlay font-desc=Sans,22 ! avenc_mpeg4 ! mpeg4videoparse ! rtpmp4vpay2 config-interval=-1 ! udpsink host=127.0.0.1 port=5004
 * ]| This will create and payload an MPEG-4 part 2 Video elementary stream with a test pattern and
 * send it out via UDP to localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;

use gst::{glib, prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basepay::{PacketToBufferRelation, RtpBasePay2Ext, RtpBasePay2ImplExt};

use crate::mp4v::pay::mpeg4_video;
use crate::mp4v::pay::mpeg4_video::{Packet, PacketType, PacketVec, VopCodingType};

use smallvec::smallvec;

use std::sync::Mutex;

#[derive(Clone, Default)]
struct Settings {
    config_interval: i32,
}

#[derive(Default)]
struct State {
    config: Vec<u8>,
    config_packets: PacketVec,
    profile_level_id: Option<u8>,
    update_caps: bool, // Output caps need to be sent or updated
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    last_config: Option<gst::ClockTime>, // Running time when we last inserted the config in-band
}

#[derive(Default)]
pub struct RtpMpeg4VideoPay {
    state: AtomicRefCell<State>,
    settings: Mutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmp4vpay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG-4 part 2 Video Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpeg4VideoPay {
    const NAME: &'static str = "GstRtpMpeg4VideoPay2";
    type Type = super::RtpMpeg4VideoPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpMpeg4VideoPay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecInt::builder("config-interval")
                    .nick("Config Interval")
                    .blurb("Regularly send config headers in-band instead of relying on external signalling (0 = disabled, -1 = on keyframes)")
                    .default_value(Settings::default().config_interval)
                    .minimum(-1)
                    .maximum(3600)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "config-interval" => {
                settings.config_interval = value.get::<i32>().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "config-interval" => settings.config_interval.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RtpMpeg4VideoPay {}

impl ElementImpl for RtpMpeg4VideoPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG-4 Part 2 Video Elementary Stream Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload an MPEG-4 part 2 Elementary Stream into RTP packets (RFC 3016)",
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
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "MP4V-ES")
                    .field("clock-rate", gst::IntRange::new(1i32, i32::MAX))
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("video/mpeg")
                            .field("systemstream", false)
                            .field("mpegversion", 4i32)
                            .field("parsed", true)
                            .build(),
                    )
                    .structure(gst::Structure::builder("video/x-divx").build())
                    .build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpMpeg4VideoPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let Ok(Some(codec_data_buf)) = s.get_optional::<gst::Buffer>("codec_data") else {
            gst::error!(
                CAT,
                imp = self,
                "MPEG-4 video caps without codec_data field, use mpeg4videoparse"
            );
            return false;
        };

        let Ok(codec_data) = codec_data_buf.map_readable() else {
            gst::error!(CAT, imp = self, "Can't map codec_data buffer readable");
            return false;
        };

        const VISUAL_OBJECT_SEQUENCE_START_LEN: usize = 5;

        if codec_data.size() < VISUAL_OBJECT_SEQUENCE_START_LEN {
            gst::error!(CAT, imp = self, "codec_data too small");
            return false;
        };

        let packets = match mpeg4_video::parse_packets_from_slice(&codec_data) {
            Ok(packets) => packets,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Could not parse codec_data buffer: {err:?}"
                );
                return false;
            }
        };

        if packets.is_empty() {
            gst::error!(CAT, imp = self, "Could not parse codec_data buffer");
            return false;
        };

        let mut state = self.state.borrow_mut();

        self.handle_headers(&mut state, &codec_data, &packets);

        // We'll send the output caps from the buffer handler, because it's possible that we
        // might get more headers in-band than we get in the codec_data, and it's best if we
        // only set the output caps once with the full headers instead of setting it now and
        // then updating it later.
        state.update_caps = true;

        true
    }

    // Encapsulation of MPEG-4 part 2 Video Elementary Streams:
    // https://datatracker.ietf.org/doc/html/rfc3016#section-3
    //
    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Can't map buffer readable");
            gst::FlowError::Error
        })?;

        let mut state = self.state.borrow_mut();

        let segment = state.segment.as_ref().unwrap();

        // Base class ensures pts or errors out if no pts on first buffer
        let buffer_running_time = segment.to_running_time(buffer.pts().expect("pts"));

        gst::trace!(
            CAT,
            imp = self,
            "Got frame with id {id}, {} bytes, buffer running time {buffer_running_time:?}",
            map.size(),
        );

        // Parse frame into packets

        let packets = match mpeg4_video::parse_packets_from_slice(&map) {
            Ok(packets) => packets,
            Err(err) => {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["Could not parse MPEG-4 video frame: {err:?}"]
                );
                return Err(gst::FlowError::Error);
            }
        };

        // Log packets

        for (i, packet) in packets.iter().enumerate() {
            gst::trace!(
                CAT,
                imp = self,
                "Buf {id}, Packet {i}: {:?} @ {}+{}",
                packet.ptype(),
                packet.offset(),
                packet.len(),
            );
        }

        // Headers are everything before GroupOfVop or Vop

        let (headers, packets) = {
            // Find the first non-header packet
            let vop = packets
                .iter()
                .position(|p| matches!(p.ptype(), PacketType::GroupOfVop | PacketType::Vop(_)));

            let Some(vop) = vop else {
                gst::element_imp_error!(self, gst::StreamError::Format, ["No MPEG-4 VOP found"]);
                return Err(gst::FlowError::Error);
            };

            packets.split_at(vop)
        };

        if !headers.is_empty() {
            self.handle_headers(&mut state, &map, headers);
        }

        // Wait for the initial config if we don't have one yet

        if state.config.is_empty() {
            gst::debug!(CAT, imp = self, "Dropping buffer - no config yet!");
            self.obj().drop_buffers(id..=id);
            return Ok(gst::FlowSuccess::Ok);
        }

        // Set output caps if needed

        if state.update_caps {
            self.set_output_caps(&mut state);
            state.update_caps = false;
        }

        // Prepare for payloading

        let is_keyframe = packets.iter().any(|p| {
            p.ptype() == PacketType::GroupOfVop || p.ptype() == PacketType::Vop(VopCodingType::I)
        });

        let config_interval = self.settings.lock().unwrap().config_interval;

        let send_config = match config_interval {
            -1 => is_keyframe,
            0 => false,
            interval => {
                let mut do_send = true;

                if let Some(last_config) = state.last_config {
                    if let Some(rt_now) = buffer_running_time {
                        do_send = (rt_now - last_config).seconds() >= interval as u64
                    } else {
                        do_send = false;
                    }
                }

                do_send
            }
        };

        // Strip or insert in-band headers depending on whether in-band header sending is disabled
        // and/or whether it's time to send in-band headers now (based either on keyframe or time).
        let headers_smallvec = if !send_config {
            smallvec![]
        } else {
            gst::debug!(
                CAT,
                imp = self,
                "Inserting in-band headers at {buffer_running_time:?}"
            );
            state.last_config = buffer_running_time;

            // The stored config should be identical to any in-band headers we may have
            // received now. Always reparse from the stored config in the interest of
            // reducing the number of possible code paths.
            mpeg4_video::parse_packets_from_slice(&state.config).expect("headers")
        };

        let headers = headers_smallvec.as_slice();

        // Payloading

        let max_payload_size = self.obj().max_payload_size() as usize;

        let mut hdr_iter = headers.iter().map(|h| h.data(&state.config)).peekable();

        let mut data_iter = packets.iter().map(|p| p.data(&map)).peekable();

        let mut rtp_packet = rtp_types::RtpPacketBuilder::new();
        let mut acc_bytes = 0;

        // Payload headers first. Only split up a header if it doesn't fit into a packet by itself
        while let Some(hdr) = hdr_iter.peek() {
            // Check if it wholly fits into the packet
            if acc_bytes + hdr.len() <= max_payload_size {
                let hdr = hdr_iter.next().unwrap();
                acc_bytes += hdr.len();
                rtp_packet = rtp_packet.payload(hdr);
                continue;
            }

            // If header's too large for a single packet split it up into multiple packets
            if hdr.len() > max_payload_size && acc_bytes == 0 {
                let hdr = hdr_iter.next().unwrap();
                while acc_bytes < hdr.len() {
                    let bytes_left_to_payload = hdr.len() - acc_bytes;
                    let bytes_to_payload = std::cmp::min(bytes_left_to_payload, max_payload_size);

                    self.obj().queue_packet(
                        PacketToBufferRelation::Ids(id..=id),
                        rtp_types::RtpPacketBuilder::new()
                            .payload(&hdr[acc_bytes..][..bytes_to_payload]),
                    )?;

                    acc_bytes += bytes_to_payload;
                }
                continue;
            }

            // .. else output what we have and start a fresh packet

            self.obj()
                .queue_packet(PacketToBufferRelation::Ids(id..=id), rtp_packet)?;

            rtp_packet = rtp_types::RtpPacketBuilder::new();
            acc_bytes = 0;
        }

        // If we don't have enough space after the headers for the start marker of a Group of Vops
        // or a Vop then output the headers now. Otherwise we'll try and pack the start of the Vop
        // directly after the headers (and any Gov) instead of starting a new RTP packet for them.
        //
        if acc_bytes + 6 > max_payload_size {
            self.obj()
                .queue_packet(PacketToBufferRelation::Ids(id..=id), rtp_packet)?;

            rtp_packet = rtp_types::RtpPacketBuilder::new();
            acc_bytes = 0;
        }

        const GOV_START_CODE: [u8; 4] = [0, 0, 1, 0xb3];

        // If we have a Gov, try and keep it together with the start of the following Vop, so
        // either try to fit both into the RTP packet currently being built or they'll both go
        // into a new RTP packet.
        //
        // | HEADERS GOV VOP.. | ..VOP..   | .. VOP .. | = good
        // | HEADERS           | GOV VOP.. | .. VOP .. | = good
        // | HEADERS GOV       | VOP..     | .. VOP .. | = avoid
        // | HEADERS           | VOP..     | .. VOP .. | = good
        // | HEADERS VOP..     | ..VOP..   | .. VOP .. | = good
        //
        // (Nothing in the RFC strictly requires this arrangement though.)
        //
        if let Some(gov) = data_iter.next_if(|&data| {
            data.starts_with(&GOV_START_CODE) && acc_bytes + data.len() + 6 <= max_payload_size
        }) {
            rtp_packet = rtp_packet.payload(gov);
            acc_bytes += gov.len();
        }

        for gov_or_vop in data_iter {
            gst::trace!(
                CAT,
                "GoV or Vop with len = {}, acc_bytes={acc_bytes}",
                gov_or_vop.len()
            );

            let mut data = gov_or_vop;

            let mut is_gov = data.starts_with(&GOV_START_CODE);

            while !data.is_empty() {
                // acc_bytes is only non-zero for the first round where we're maybe packing things
                // into an in-progress RTP packet that already contains some headers.
                let bytes_to_payload = std::cmp::min(data.len(), max_payload_size - acc_bytes);

                let is_last = bytes_to_payload == data.len();

                rtp_packet = rtp_packet
                    .payload(&data[0..][..bytes_to_payload])
                    .marker_bit(is_last);

                // Group Gov + Vop together in the same RTP packet if possible at all
                if is_gov && acc_bytes + bytes_to_payload + 6 <= max_payload_size {
                    acc_bytes += bytes_to_payload;
                } else {
                    self.obj()
                        .queue_packet(PacketToBufferRelation::Ids(id..=id), rtp_packet)?;

                    rtp_packet = rtp_types::RtpPacketBuilder::new();
                    acc_bytes = 0;
                }

                data = &data[bytes_to_payload..];
                is_gov = false;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, event: gst::Event) -> Result<gst::FlowSuccess, gst::FlowError> {
        #[allow(clippy::single_match)]
        match event.view() {
            gst::EventView::Segment(ev) => {
                // Chain up first so base class can do error checking and such
                self.parent_sink_event(event.clone())?;

                let mut state = self.state.borrow_mut();

                let segment = ev.segment().clone().downcast::<gst::ClockTime>().unwrap();

                gst::info!(CAT, imp = self, "Segment: {segment:?}");

                state.segment = Some(segment.clone());

                Ok(gst::FlowSuccess::Ok)
            }
            _ => self.parent_sink_event(event),
        }
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();

        *state = State::default();

        // Make sure configured MTU is large enough
        let mtu = self.obj().mtu() as usize;

        // RFC says headers should fit into a single RTP packets, so require some sensible minimum
        // size, but the actual number is completely made up (taken from MPEG-2 payloader for now).
        if mtu < 278 {
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ("Configured MTU is too small")
            ));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }
}

impl RtpMpeg4VideoPay {
    fn handle_headers(&self, state: &mut State, data: &[u8], headers: &[Packet]) {
        assert!(!headers.is_empty());

        for (i, header) in headers.iter().enumerate() {
            gst::trace!(
                CAT,
                imp = self,
                "Header {i}: {:?} @ {}+{}",
                header.ptype(),
                header.offset(),
                header.len(),
            );

            if let PacketType::VisualObjectSequenceStart(id) = header.ptype() {
                gst::info!(CAT, imp = self, "profile_level_id = {id}");
                state.profile_level_id = Some(id);
            }
        }

        let start = headers.first().unwrap().offset();
        let end = headers.last().unwrap().offset() + headers.last().unwrap().len();

        let new_config = &data[start..end];

        let config_changed = new_config != state.config;

        if config_changed {
            gst::debug!(
                CAT,
                imp = self,
                "Config changed from {:?} to {:?}",
                hex::encode(&state.config),
                hex::encode(new_config),
            );
            state.config.clear();
            state.config.extend_from_slice(new_config);
            state.config_packets.clear();
            state.config_packets.extend_from_slice(headers);
            state.update_caps = true;
        } else {
            gst::log!(CAT, imp = self, "New config is same as old config");
        }
    }

    fn set_output_caps(&self, state: &mut State) {
        // Caller ensures that already
        assert!(!state.config.is_empty());

        let profile_level_id = state.profile_level_id.unwrap_or(1); // 1 = Simple Profile, Level 1

        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "MP4V-ES")
            .field("clock-rate", 90000i32)
            .field("config", hex::encode(&state.config))
            .field("profile-level-id", profile_level_id.to_string())
            .build();

        gst::info!(CAT, imp = self, "Setting output caps {src_caps}..");

        // We'll ignore any failure here and let the next buffer push return the right flow error
        self.obj().set_src_caps(&src_caps);
    }
}
