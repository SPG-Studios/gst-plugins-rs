//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph266depay
 * @see_also: rtph266pay, h266parse, avdec_h266
 *
 * Depayload H266 (VVC) from RTP packets per RFC 9328.
 *
 * Outputs byte-stream (Annex-B), AU-aligned H266:
 *
 * |[
 * gst-launch-1.0 udpsrc caps='application/x-rtp,media=video,encoding-name=H266,clock-rate=90000' ! \
 *   rtph266depay ! h266parse ! avdec_h266 ! videoconvert ! autovideosink
 * ]|
 *
 * Parameter sets supplied out-of-band on the input caps as
 * `sprop-vps`/`sprop-sps`/`sprop-pps` (base64) are prepended to the first
 * output access unit. The #GstRtpH266Depay:wait-for-keyframe and
 * #GstRtpH266Depay:request-keyframe properties control the behaviour after
 * packet loss.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;
use gst::{glib, prelude::*, subclass::prelude::*};
use std::sync::LazyLock;
use std::sync::Mutex;

use crate::basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext};
use crate::h266::common::*;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph266depay",
        gst::DebugColorFlags::empty(),
        Some("RTP H266 Depayloader"),
    )
});

const ANNEXB_START_CODE: &[u8] = &[0, 0, 0, 1];

#[derive(Clone, Copy, Default)]
struct Settings {
    request_keyframe: bool,
    wait_for_keyframe: bool,
}

#[derive(Default)]
struct State {
    /// Accumulated Annex-B data for the current access unit.
    pending_au: Vec<u8>,
    /// Extended seqnum of the first packet contributing to the pending AU.
    au_start_ext_seqnum: Option<u64>,
    /// Extended RTP timestamp of the pending AU.
    au_ext_timestamp: Option<u64>,
    /// Whether the pending AU contains an IRAP NAL (keyframe).
    au_has_irap: bool,
    /// FU reassembly buffer (starts with the reconstructed 2-byte header).
    fu_buffer: Option<Vec<u8>>,
    /// Extended seqnum of the last packet we processed (gap detection).
    last_ext_seqnum: Option<u64>,
    /// Hold output back until the next IRAP (at start / after loss when
    /// wait-for-keyframe is enabled).
    waiting_for_keyframe: bool,
    /// Out-of-band parameter sets from the input caps (sprop-*), in
    /// VPS/SPS/PPS order, prepended to the first emitted AU.
    oob_param_sets: Vec<Vec<u8>>,
    /// Whether the out-of-band parameter sets are still to be emitted.
    oob_pending: bool,
}

#[derive(Default)]
pub struct RtpH266Depay {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
}

impl RtpH266Depay {
    fn reset_fu(&self, state: &mut State) {
        state.fu_buffer = None;
    }

    fn reset(&self, state: &mut State) {
        let oob = std::mem::take(&mut state.oob_param_sets);
        let oob_pending = !oob.is_empty();
        *state = State::default();
        state.oob_param_sets = oob;
        state.oob_pending = oob_pending;
        state.waiting_for_keyframe = self.settings.lock().unwrap().wait_for_keyframe;
    }

    /// Drop the current incomplete AU and, per the configured policy, request
    /// a keyframe upstream and/or hold output until the next keyframe.
    fn handle_loss(&self, state: &mut State) {
        state.pending_au.clear();
        state.au_start_ext_seqnum = None;
        state.au_ext_timestamp = None;
        state.au_has_irap = false;
        self.reset_fu(state);

        let settings = *self.settings.lock().unwrap();
        if settings.request_keyframe {
            gst::debug!(CAT, imp = self, "requesting keyframe from upstream");
            let event = gst_video::UpstreamForceKeyUnitEvent::builder()
                .all_headers(true)
                .build();
            let _ = self.obj().sink_pad().push_event(event);
        }
        if settings.wait_for_keyframe {
            state.waiting_for_keyframe = true;
        }
    }

    /// Append one complete NAL unit (without start code) to the pending AU.
    fn push_nal(&self, state: &mut State, nal: &[u8]) {
        if let Some(hdr) = NalHeader::parse(nal)
            && hdr.is_irap()
        {
            state.au_has_irap = true;
        }
        state.pending_au.extend_from_slice(ANNEXB_START_CODE);
        state.pending_au.extend_from_slice(nal);
    }

    /// Push the pending AU downstream.
    fn finish_au(
        &self,
        state: &mut State,
        end_ext_seqnum: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if state.pending_au.is_empty() {
            state.au_start_ext_seqnum = None;
            state.au_ext_timestamp = None;
            state.au_has_irap = false;
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut au = std::mem::take(&mut state.pending_au);
        let start = state.au_start_ext_seqnum.take().unwrap_or(end_ext_seqnum);
        let has_irap = std::mem::take(&mut state.au_has_irap);
        state.au_ext_timestamp = None;

        // Hold output until the next IRAP if requested.
        if state.waiting_for_keyframe {
            if has_irap {
                state.waiting_for_keyframe = false;
            } else {
                gst::trace!(CAT, imp = self, "waiting for keyframe, discarding AU");
                self.obj().drop_packets(..=end_ext_seqnum);
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        // Prepend any out-of-band parameter sets to the first emitted AU.
        if state.oob_pending {
            let mut prefixed = Vec::with_capacity(au.len() + state.oob_param_sets.len() * 8);
            for ps in &state.oob_param_sets {
                prefixed.extend_from_slice(ANNEXB_START_CODE);
                prefixed.extend_from_slice(ps);
            }
            prefixed.append(&mut au);
            au = prefixed;
            state.oob_pending = false;
        }

        gst::trace!(
            CAT,
            imp = self,
            "finishing AU: {} bytes, keyframe={has_irap}",
            au.len()
        );

        let mut buffer = gst::Buffer::from_mut_slice(au);
        if !has_irap {
            buffer
                .get_mut()
                .unwrap()
                .set_flags(gst::BufferFlags::DELTA_UNIT);
        }

        self.obj().queue_buffer(
            PacketToBufferRelation::Seqnums(start..=end_ext_seqnum),
            buffer,
        )
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH266Depay {
    const NAME: &'static str = "GstRtpH266Depay";
    type Type = super::RtpH266Depay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpH266Depay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("request-keyframe")
                    .nick("Request Keyframe")
                    .blurb("Request a new keyframe upstream when packet loss is detected")
                    .default_value(Settings::default().request_keyframe)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("wait-for-keyframe")
                    .nick("Wait For Keyframe")
                    .blurb("Wait for the next keyframe after packet loss")
                    .default_value(Settings::default().wait_for_keyframe)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "request-keyframe" => {
                self.settings.lock().unwrap().request_keyframe = value.get().unwrap();
            }
            "wait-for-keyframe" => {
                self.settings.lock().unwrap().wait_for_keyframe = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "request-keyframe" => self.settings.lock().unwrap().request_keyframe.to_value(),
            "wait-for-keyframe" => self.settings.lock().unwrap().wait_for_keyframe.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RtpH266Depay {}

impl ElementImpl for RtpH266Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H266 depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload H266 (VVC) from RTP packets (RFC 9328)",
                "Sanil Raut <sr1990003@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", CLOCK_RATE)
                    .field("encoding-name", "H266")
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/x-h266")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpH266Depay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.reset(&mut self.state.borrow_mut());
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn flush(&self) {
        let mut state = self.state.borrow_mut();
        state.pending_au.clear();
        state.au_start_ext_seqnum = None;
        state.au_ext_timestamp = None;
        state.au_has_irap = false;
        state.last_ext_seqnum = None;
        self.reset_fu(&mut state);
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        // basedepay2 calls drain() on an upstream discontinuity (flush, segment
        // change, or detected packet loss). Any partially-assembled access unit
        // is now unreliable, so drop it and apply the loss-recovery policy.
        let mut state = self.state.borrow_mut();
        if !state.pending_au.is_empty() || state.fu_buffer.is_some() {
            gst::debug!(
                CAT,
                imp = self,
                "drain on discontinuity, discarding partial AU ({} bytes)",
                state.pending_au.len()
            );
        }
        self.handle_loss(&mut state);
        Ok(gst::FlowSuccess::Ok)
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        // Read out-of-band parameter sets (sprop-*) from the input caps.
        let mut oob = vec![];
        if let Some(s) = caps.structure(0) {
            for field in ["sprop-vps", "sprop-sps", "sprop-pps"] {
                if let Ok(b64) = s.get::<&str>(field) {
                    let decoded = glib::base64_decode(b64);
                    if NalHeader::parse(&decoded).is_some() {
                        oob.push(decoded);
                    }
                }
            }
        }
        {
            let mut state = self.state.borrow_mut();
            state.oob_pending = !oob.is_empty();
            state.oob_param_sets = oob;
        }

        let src_caps = gst::Caps::builder("video/x-h266")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let payload = packet.payload();
        let Some(hdr) = NalHeader::parse(payload) else {
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        };

        if hdr.forbidden {
            gst::warning!(CAT, imp = self, "F bit set, dropping packet");
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // Sequence number gap: at least one packet of the current AU (or its
        // marker) was lost. The pending AU is incomplete.
        if state
            .last_ext_seqnum
            .is_some_and(|last| packet.ext_seqnum() != last + 1)
        {
            gst::debug!(
                CAT,
                imp = self,
                "seqnum gap ({:?} -> {})",
                state.last_ext_seqnum,
                packet.ext_seqnum()
            );
            self.handle_loss(&mut state);
        }
        state.last_ext_seqnum = Some(packet.ext_seqnum());

        // A new RTP timestamp without having seen the marker means the marker
        // packet was lost. The pending AU is unreliable.
        if state
            .au_ext_timestamp
            .is_some_and(|t| t != packet.ext_timestamp())
        {
            gst::debug!(
                CAT,
                imp = self,
                "timestamp change without marker, dropping incomplete AU"
            );
            self.handle_loss(&mut state);
        }

        if state.au_start_ext_seqnum.is_none() {
            state.au_start_ext_seqnum = Some(packet.ext_seqnum());
        }
        state.au_ext_timestamp = Some(packet.ext_timestamp());

        match hdr.nal_type {
            RTP_TYPE_FU => {
                if payload.len() < NAL_HEADER_SIZE + FU_HEADER_SIZE + 1 {
                    self.obj().drop_packet(packet);
                    return Ok(gst::FlowSuccess::Ok);
                }
                let fu = FuHeader::parse(payload[2]);
                let fu_payload = &payload[NAL_HEADER_SIZE + FU_HEADER_SIZE..];

                if fu.start {
                    // Reconstruct the original NAL header: byte 0 unchanged,
                    // byte 1 = original type in bits 7..3 + original TID.
                    let orig = NalHeader {
                        nal_type: fu.fu_type,
                        ..hdr
                    };
                    let mut buf = Vec::with_capacity(4096);
                    buf.extend_from_slice(&orig.to_bytes());
                    state.fu_buffer = Some(buf);
                }

                let Some(buf) = state.fu_buffer.as_mut() else {
                    gst::debug!(CAT, imp = self, "FU without start, dropping");
                    self.obj().drop_packet(packet);
                    return Ok(gst::FlowSuccess::Ok);
                };
                buf.extend_from_slice(fu_payload);

                if fu.end {
                    let nal = state.fu_buffer.take().unwrap();
                    gst::trace!(
                        CAT,
                        imp = self,
                        "FU reassembled: type={} {} bytes",
                        fu.fu_type,
                        nal.len()
                    );
                    self.push_nal(&mut state, &nal);
                }
            }
            RTP_TYPE_AP => {
                // Append each aggregated NAL unit directly (one copy into the
                // pending AU; no intermediate per-unit allocation).
                let mut units = 0;
                for nal in iter_ap_units(payload) {
                    self.push_nal(&mut state, nal);
                    units += 1;
                }
                gst::trace!(CAT, imp = self, "AP with {units} NAL units");
                if units == 0 {
                    self.obj().drop_packet(packet);
                    return Ok(gst::FlowSuccess::Ok);
                }
            }
            _ => {
                // Single NAL unit packet — append directly (one copy).
                self.push_nal(&mut state, payload);
            }
        }

        if packet.marker_bit() {
            self.finish_au(&mut state, packet.ext_seqnum())?;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
