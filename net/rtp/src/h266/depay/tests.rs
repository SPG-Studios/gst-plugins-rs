//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{Caps, event::Eos, prelude::*};
use gst_check::Harness;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtph266 test");
    });
}

const NAL_TRAIL: u8 = 1;
const NAL_IDR_W_RADL: u8 = 7;
const NAL_VPS: u8 = 14;
const NAL_SPS: u8 = 15;
const NAL_PPS: u8 = 16;

const RTP_TYPE_AP: u8 = 28;
const RTP_TYPE_FU: u8 = 29;

const START_CODE: &[u8] = &[0, 0, 0, 1];

fn nal(ty: u8, layer: u8, tid: u8, len: usize) -> Vec<u8> {
    assert!(len >= 2);
    let mut n = vec![layer & 0x3f, ((ty & 0x1f) << 3) | (tid & 0x7)];
    for i in 0..len - 2 {
        n.push((i % 251) as u8);
    }
    n
}

fn fu_header(start: bool, end: bool, ty: u8) -> u8 {
    let mut b = ty & 0x1f;
    if start {
        b |= 0x80;
    }
    if end {
        b |= 0x40;
    }
    b
}

fn make_harness() -> Harness {
    init();
    let mut h = Harness::new("rtph266depay");
    h.play();
    let caps = Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("payload", 96)
        .field("clock-rate", 90000)
        .field("encoding-name", "H266")
        .build();
    h.set_src_caps(caps);
    h
}

/// Push one RTP packet (raw payload bytes) with the given seq/marker.
fn push_rtp(h: &mut Harness, payload: &[u8], seq: u16, marker: bool, ts: u32) {
    let buf = rtp_types::RtpPacketBuilder::new()
        .marker_bit(marker)
        .timestamp(ts)
        .payload_type(96)
        .sequence_number(seq)
        .payload(payload)
        .write_vec()
        .unwrap();
    h.push(gst::Buffer::from_mut_slice(buf)).unwrap();
}

/// Pull all output buffers as (bytes, is_keyframe) pairs.
fn pull_buffers(h: &mut Harness) -> Vec<(Vec<u8>, bool)> {
    let mut out = vec![];
    while let Some(buffer) = h.try_pull() {
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let map = buffer.map_readable().unwrap();
        out.push((map.to_vec(), keyframe));
    }
    out
}

#[test]
fn test_single_nal() {
    let mut h = make_harness();
    let trail = nal(NAL_TRAIL, 0, 1, 16);
    push_rtp(&mut h, &trail, 0, true, 1000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1);
    let mut expected = START_CODE.to_vec();
    expected.extend_from_slice(&trail);
    assert_eq!(bufs[0].0, expected);
    assert!(!bufs[0].1, "TRAIL is not a keyframe");
}

#[test]
fn test_single_nal_keyframe_flag() {
    let mut h = make_harness();
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);
    push_rtp(&mut h, &idr, 0, true, 1000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1);
    assert!(
        bufs[0].1,
        "IDR AU must be flagged as a keyframe (no DELTA_UNIT)"
    );
}

#[test]
fn test_aggregation_packet() {
    let mut h = make_harness();
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);

    // Build an AP payload: [AP hdr(2)] [size][nal] x3
    let mut ap = vec![0u8, (RTP_TYPE_AP << 3) | 1];
    for u in [&vps, &sps, &pps] {
        ap.extend_from_slice(&(u.len() as u16).to_be_bytes());
        ap.extend_from_slice(u);
    }
    push_rtp(&mut h, &ap, 0, true, 1000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1, "one AU buffer");
    let mut expected = vec![];
    for u in [&vps, &sps, &pps] {
        expected.extend_from_slice(START_CODE);
        expected.extend_from_slice(u);
    }
    assert_eq!(bufs[0].0, expected, "all aggregated NALs in Annex-B order");
}

#[test]
fn test_fragmentation_reassembly() {
    let mut h = make_harness();
    // Original NAL: IDR, payload split across 3 FUs.
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 30);
    let payload = &idr[2..];
    let third = payload.len() / 3;

    let fu_b0 = idr[0];
    let fu_b1 = (RTP_TYPE_FU << 3) | (idr[1] & 0x7);

    let mk_fu = |start: bool, end: bool, chunk: &[u8]| {
        let mut p = vec![fu_b0, fu_b1, fu_header(start, end, NAL_IDR_W_RADL)];
        p.extend_from_slice(chunk);
        p
    };

    push_rtp(
        &mut h,
        &mk_fu(true, false, &payload[..third]),
        0,
        false,
        1000,
    );
    push_rtp(
        &mut h,
        &mk_fu(false, false, &payload[third..2 * third]),
        1,
        false,
        1000,
    );
    push_rtp(
        &mut h,
        &mk_fu(false, true, &payload[2 * third..]),
        2,
        true,
        1000,
    );
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1);
    let mut expected = START_CODE.to_vec();
    expected.extend_from_slice(&idr);
    assert_eq!(bufs[0].0, expected, "FU reassembled to the original NAL");
    assert!(bufs[0].1, "reassembled IDR is a keyframe");
}

#[test]
fn test_multiple_aus() {
    let mut h = make_harness();
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);
    let trail = nal(NAL_TRAIL, 0, 1, 16);

    // Two AUs at different timestamps, each a single NAL with marker.
    push_rtp(&mut h, &idr, 0, true, 1000);
    push_rtp(&mut h, &trail, 1, true, 4000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 2);
    assert!(bufs[0].1, "first AU is a keyframe (IDR)");
    assert!(!bufs[1].1, "second AU is a delta (TRAIL)");
}

#[test]
fn test_too_short_packet_dropped() {
    let mut h = make_harness();
    // A 1-byte payload is shorter than the NAL header — must be dropped,
    // and the following valid NAL still delivered.
    push_rtp(&mut h, &[0x00], 0, false, 1000);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);
    push_rtp(&mut h, &idr, 1, true, 1000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1, "only the valid AU is delivered");
    assert!(bufs[0].1);
}

#[test]
fn test_sprop_bootstrap() {
    init();
    let mut h = Harness::new("rtph266depay");
    h.play();

    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    // Supply parameter sets out-of-band on the input caps.
    let caps = Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("payload", 96)
        .field("clock-rate", 90000)
        .field("encoding-name", "H266")
        .field("sprop-sps", glib::base64_encode(&sps))
        .field("sprop-pps", glib::base64_encode(&pps))
        .build();
    h.set_src_caps(caps);

    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);
    push_rtp(&mut h, &idr, 0, true, 1000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    assert_eq!(bufs.len(), 1);
    // The first AU must be prefixed with the out-of-band SPS and PPS.
    let mut expected = vec![];
    for u in [&sps, &pps, &idr] {
        expected.extend_from_slice(START_CODE);
        expected.extend_from_slice(u);
    }
    assert_eq!(
        bufs[0].0, expected,
        "sprop param sets prepended to first AU"
    );
    assert!(bufs[0].1, "first AU is a keyframe");
}

#[test]
fn test_wait_for_keyframe_holds_until_idr() {
    init();
    let mut h = Harness::new("rtph266depay");
    h.element().unwrap().set_property("wait-for-keyframe", true);
    h.play();
    let caps = Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("payload", 96)
        .field("clock-rate", 90000)
        .field("encoding-name", "H266")
        .build();
    h.set_src_caps(caps);

    let trail = nal(NAL_TRAIL, 0, 1, 16);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);
    // A non-keyframe AU before the first keyframe must be held back.
    push_rtp(&mut h, &trail, 0, true, 1000);
    push_rtp(&mut h, &idr, 1, true, 4000);
    push_rtp(&mut h, &trail, 2, true, 7000);
    h.push_event(Eos::new());

    let bufs = pull_buffers(&mut h);
    // First TRAIL dropped (no keyframe yet); IDR + following TRAIL pass.
    assert_eq!(bufs.len(), 2);
    assert!(bufs[0].1, "first delivered AU is the keyframe");
    assert!(!bufs[1].1, "subsequent delta passes once keyframe seen");
}

#[test]
fn test_property_roundtrip() {
    init();
    let h = Harness::new("rtph266depay");
    let depay = h.element().unwrap();
    assert!(!depay.property::<bool>("wait-for-keyframe"));
    assert!(!depay.property::<bool>("request-keyframe"));
    depay.set_property("wait-for-keyframe", true);
    depay.set_property("request-keyframe", true);
    assert!(depay.property::<bool>("wait-for-keyframe"));
    assert!(depay.property::<bool>("request-keyframe"));
}
