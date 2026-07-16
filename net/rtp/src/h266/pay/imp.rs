//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph266pay
 * @see_also: rtph266depay, h266parse
 *
 * Payload an H266 (VVC) video stream into RTP packets per RFC 9328.
 *
 * Expects byte-stream (Annex-B), AU-aligned input from `h266parse`:
 *
 * |[
 * gst-launch-1.0 filesrc location=clip.266 ! h266parse ! rtph266pay ! udpsink
 * ]|
 *
 * NAL units larger than the MTU are split into Fragmentation Units (type 29).
 * The #GstRtpH266Pay:aggregate-mode property controls aggregation of small NAL
 * units into Aggregation Packets (type 28); #GstRtpH266Pay:config-interval
 * re-inserts the parameter sets ahead of keyframes so that receivers joining
 * mid-stream can start decoding. The current parameter sets are also exported
 * out-of-band on the output caps as `sprop-vps`/`sprop-sps`/`sprop-pps`.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;
use gst::{glib, prelude::*, subclass::prelude::*};
use std::sync::LazyLock;
use std::sync::Mutex;

use super::AggregateMode;
use crate::basepay::RtpBasePay2Ext;
use crate::h266::common::*;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph266pay",
        gst::DebugColorFlags::empty(),
        Some("RTP H266 Payloader"),
    )
});

const DEFAULT_CONFIG_INTERVAL: i32 = 0;

#[derive(Clone, Copy)]
struct Settings {
    /// VPS/SPS/PPS re-insertion interval in seconds:
    /// 0 = disabled (only sent when present in-stream),
    /// -1 = before every keyframe AU,
    /// N>0 = before a keyframe AU when at least N seconds have elapsed.
    config_interval: i32,
    aggregate_mode: AggregateMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            config_interval: DEFAULT_CONFIG_INTERVAL,
            aggregate_mode: AggregateMode::default(),
        }
    }
}

#[derive(Default)]
struct State {
    /// Latest known parameter sets, retained for config-interval re-insertion
    /// and out-of-band (sprop-*) advertisement.
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// PTS at which parameter sets were last (re-)inserted for config-interval.
    last_config_time: Option<gst::ClockTime>,
    /// Whether the current parameter sets are already advertised on src caps.
    sprop_advertised: bool,
}

#[derive(Default)]
pub struct RtpH266Pay {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
}

/// Whether parameter sets should be (re-)inserted for this AU based on the
/// config-interval policy.
fn config_due(
    interval: i32,
    keyframe: bool,
    pts: Option<gst::ClockTime>,
    last: Option<gst::ClockTime>,
) -> bool {
    if !keyframe {
        return false;
    }
    match interval {
        i if i < 0 => true, // -1: every keyframe AU
        0 => false,
        secs => match (pts, last) {
            (Some(pts), Some(last)) => {
                pts.saturating_sub(last) >= gst::ClockTime::from_seconds(secs as u64)
            }
            // No timing reference (or first keyframe) -> send.
            _ => true,
        },
    }
}

impl RtpH266Pay {
    /// Payload one NAL unit as a single RTP packet, or as Fragmentation Units
    /// when it exceeds the MTU.
    fn emit_single_or_fu(&self, nal: &[u8], max_payload: usize, out: &mut Vec<Vec<u8>>) {
        let Some(hdr) = NalHeader::parse(nal) else {
            return;
        };

        if nal.len() <= max_payload {
            out.push(nal.to_vec());
            return;
        }

        let fu_overhead = NAL_HEADER_SIZE + FU_HEADER_SIZE;
        if max_payload <= fu_overhead {
            gst::warning!(CAT, imp = self, "MTU too small to fragment NAL");
            return;
        }

        // FU payload header: original NAL header with Type replaced by 29.
        let fu_payload_hdr = NalHeader {
            nal_type: RTP_TYPE_FU,
            ..hdr
        }
        .to_bytes();
        let payload = &nal[NAL_HEADER_SIZE..];
        let max_fragment = max_payload - fu_overhead;

        let mut offset = 0;
        let mut fragments = 0;
        while offset < payload.len() {
            let first = offset == 0;
            let take = (payload.len() - offset).min(max_fragment);
            let end = offset + take == payload.len();

            let mut pkt = Vec::with_capacity(fu_overhead + take);
            pkt.extend_from_slice(&fu_payload_hdr);
            pkt.push(FuHeader::new(first, end, hdr.nal_type).to_byte());
            pkt.extend_from_slice(&payload[offset..offset + take]);
            out.push(pkt);

            offset += take;
            fragments += 1;
        }

        gst::trace!(
            CAT,
            imp = self,
            "FU (type 29): NAL type={} {} bytes -> {fragments} fragments",
            hdr.nal_type,
            nal.len()
        );
    }

    /// Flush a pending aggregation bundle: nothing for 0 units, a single NAL
    /// packet for 1 unit, an AP (or single-NAL fallback) for 2+.
    fn flush_bundle(&self, bundle: &mut Vec<&[u8]>, max_payload: usize, out: &mut Vec<Vec<u8>>) {
        match bundle.len() {
            0 => {}
            1 => out.push(bundle[0].to_vec()),
            _ => {
                if let Some(ap) = build_ap(bundle, max_payload) {
                    gst::trace!(CAT, imp = self, "AP (type 28) of {} NALs", bundle.len());
                    out.push(ap);
                } else {
                    for nal in bundle.iter() {
                        out.push(nal.to_vec());
                    }
                }
            }
        }
        bundle.clear();
    }

    /// Payload the ordered `nals` of an access unit according to `mode`.
    fn emit_nals(
        &self,
        nals: &[&[u8]],
        mode: AggregateMode,
        max_payload: usize,
        out: &mut Vec<Vec<u8>>,
    ) {
        match mode {
            AggregateMode::None => {
                for nal in nals {
                    self.emit_single_or_fu(nal, max_payload, out);
                }
            }
            AggregateMode::ZeroLatency => {
                // Aggregate consecutive non-VCL NALs; flush ahead of each VCL
                // NAL (which is then sent as single/FU). No latency is added
                // since the whole access unit is available at once.
                let mut bundle: Vec<&[u8]> = vec![];
                for &nal in nals {
                    let is_vcl = NalHeader::parse(nal).is_some_and(|h| h.is_vcl());
                    if is_vcl || nal.len() > max_payload {
                        self.flush_bundle(&mut bundle, max_payload, out);
                        self.emit_single_or_fu(nal, max_payload, out);
                    } else {
                        bundle.push(nal);
                    }
                }
                self.flush_bundle(&mut bundle, max_payload, out);
            }
            AggregateMode::Max => {
                // Greedily bundle any NALs that fit the MTU.
                let mut bundle: Vec<&[u8]> = vec![];
                let mut bundle_size = NAL_HEADER_SIZE;
                for &nal in nals {
                    if nal.len() > max_payload {
                        self.flush_bundle(&mut bundle, max_payload, out);
                        bundle_size = NAL_HEADER_SIZE;
                        self.emit_single_or_fu(nal, max_payload, out);
                        continue;
                    }
                    if !bundle.is_empty() && bundle_size + 2 + nal.len() > max_payload {
                        self.flush_bundle(&mut bundle, max_payload, out);
                        bundle_size = NAL_HEADER_SIZE;
                    }
                    bundle.push(nal);
                    bundle_size += 2 + nal.len();
                }
                self.flush_bundle(&mut bundle, max_payload, out);
            }
        }
    }

    /// Advertise the retained parameter sets out-of-band on the src caps as
    /// `sprop-vps`/`sprop-sps`/`sprop-pps` (base64), if they changed.
    fn update_sprop_caps(&self, state: &mut State) {
        if state.sprop_advertised || state.sps.is_none() || state.pps.is_none() {
            return;
        }

        let mut caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", CLOCK_RATE)
            .field("encoding-name", "H266");
        if let Some(vps) = &state.vps {
            caps = caps.field("sprop-vps", glib::base64_encode(vps));
        }
        if let Some(sps) = &state.sps {
            caps = caps.field("sprop-sps", glib::base64_encode(sps));
        }
        if let Some(pps) = &state.pps {
            caps = caps.field("sprop-pps", glib::base64_encode(pps));
        }
        self.obj().set_src_caps(&caps.build());
        state.sprop_advertised = true;
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH266Pay {
    const NAME: &'static str = "GstRtpH266Pay";
    type Type = super::RtpH266Pay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpH266Pay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecInt::builder("config-interval")
                    .nick("Config Interval")
                    .blurb(
                        "Re-insert VPS/SPS/PPS ahead of keyframes (in seconds); \
                         0 = only when present in-stream, -1 = with every keyframe",
                    )
                    .minimum(-1)
                    .maximum(3600)
                    .default_value(DEFAULT_CONFIG_INTERVAL)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder::<AggregateMode>("aggregate-mode")
                    .nick("Aggregate Mode")
                    .blurb("Whether and how to aggregate NAL units into Aggregation Packets")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "config-interval" => {
                self.settings.lock().unwrap().config_interval =
                    value.get().expect("type checked upstream");
            }
            "aggregate-mode" => {
                self.settings.lock().unwrap().aggregate_mode =
                    value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "config-interval" => self.settings.lock().unwrap().config_interval.to_value(),
            "aggregate-mode" => self.settings.lock().unwrap().aggregate_mode.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RtpH266Pay {}

impl ElementImpl for RtpH266Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H266 payloader",
                "Codec/Payloader/Network/RTP",
                "Payload H266 (VVC) as RTP packets (RFC 9328)",
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
                &gst::Caps::builder("video/x-h266")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("clock-rate", CLOCK_RATE)
                    .field("encoding-name", "H266")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpH266Pay {
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

        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", CLOCK_RATE)
            .field("encoding-name", "H266")
            .build();
        self.obj().set_src_caps(&caps);

        true
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let max_payload = self.obj().max_payload_size() as usize;
        let (config_interval, aggregate_mode) = {
            let settings = self.settings.lock().unwrap();
            (settings.config_interval, settings.aggregate_mode)
        };
        let pts = buffer.pts();

        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        let mut state = self.state.borrow_mut();

        // Pass 1: classify the AU and update the retained parameter sets.
        let mut au_has_param = false;
        let mut au_is_keyframe = false;
        for nal in iter_nals(map.as_slice()) {
            let Some(hdr) = NalHeader::parse(nal) else {
                continue;
            };
            match hdr.nal_type {
                NAL_TYPE_VPS => {
                    if state.vps.as_deref() != Some(nal) {
                        state.vps = Some(nal.to_vec());
                        state.sprop_advertised = false;
                    }
                    au_has_param = true;
                }
                NAL_TYPE_SPS => {
                    if state.sps.as_deref() != Some(nal) {
                        state.sps = Some(nal.to_vec());
                        state.sprop_advertised = false;
                    }
                    au_has_param = true;
                }
                NAL_TYPE_PPS => {
                    if state.pps.as_deref() != Some(nal) {
                        state.pps = Some(nal.to_vec());
                        state.sprop_advertised = false;
                    }
                    au_has_param = true;
                }
                _ if hdr.is_irap() => au_is_keyframe = true,
                _ => {}
            }
        }

        // Advertise the parameter sets out-of-band (sprop-*) if they changed.
        self.update_sprop_caps(&mut state);

        let want_config = config_due(config_interval, au_is_keyframe, pts, state.last_config_time);

        // Build the ordered list of NAL units to emit for this access unit.
        // If config-interval asks to re-insert the parameter sets ahead of
        // this keyframe and they are not already present in-stream, prepend
        // the retained copies.
        let mut nals: Vec<&[u8]> = vec![];
        if want_config && !au_has_param {
            for ps in [&state.vps, &state.sps, &state.pps].into_iter().flatten() {
                nals.push(ps.as_slice());
            }
        }
        for nal in iter_nals(map.as_slice()) {
            match NalHeader::parse(nal) {
                Some(hdr) if hdr.is_aud_or_filler() => continue,
                Some(_) => nals.push(nal),
                None => continue,
            }
        }

        let mut payloads: Vec<Vec<u8>> = vec![];
        self.emit_nals(&nals, aggregate_mode, max_payload, &mut payloads);

        if want_config && pts.is_some() {
            state.last_config_time = pts;
        }
        drop(state);

        gst::trace!(
            CAT,
            imp = self,
            "AU of {} bytes -> {} RTP payloads (keyframe={au_is_keyframe})",
            map.size(),
            payloads.len()
        );

        let n = payloads.len();
        for (i, payload) in payloads.iter().enumerate() {
            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .marker_bit(i + 1 == n)
                    .payload(payload.as_slice()),
            )?;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
