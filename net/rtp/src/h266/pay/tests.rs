//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{Buffer, Caps, ClockTime, event::Eos, prelude::*};
use gst_check::Harness;

use super::AggregateMode;

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
const NAL_PREFIX_APS: u8 = 17;
const NAL_VPS: u8 = 14;
const NAL_SPS: u8 = 15;
const NAL_PPS: u8 = 16;
const NAL_AUD: u8 = 20;
const NAL_FD: u8 = 25;

const RTP_TYPE_AP: u8 = 28;
const RTP_TYPE_FU: u8 = 29;

fn nal(ty: u8, layer: u8, tid: u8, len: usize) -> Vec<u8> {
    assert!(len >= 2);
    let mut n = vec![layer & 0x3f, ((ty & 0x1f) << 3) | (tid & 0x7)];
    for i in 0..len - 2 {
        n.push((i % 251) as u8);
    }
    n
}

fn nal_type(b1: u8) -> u8 {
    (b1 >> 3) & 0x1f
}

fn fu_type(b: u8) -> u8 {
    b & 0x1f
}

fn au(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![];
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

fn push_au(h: &mut Harness, bytes: &[u8], pts: ClockTime, keyframe: bool) {
    let mut buffer = Buffer::with_size(bytes.len())
        .unwrap()
        .into_mapped_buffer_writable()
        .unwrap();
    buffer.copy_from_slice(bytes);
    let mut buffer = buffer.into_buffer();
    buffer.get_mut().unwrap().set_pts(pts);
    if !keyframe {
        buffer
            .get_mut()
            .unwrap()
            .set_flags(gst::BufferFlags::DELTA_UNIT);
    }
    h.push(buffer).unwrap();
}

fn make_harness(mtu: u32, mode: AggregateMode) -> Harness {
    init();
    let mut h = Harness::new("rtph266pay");
    h.element().unwrap().set_property("mtu", mtu);
    h.element().unwrap().set_property("aggregate-mode", mode);
    h.play();
    let caps = Caps::builder("video/x-h266")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);
    h
}

fn pull_packets(h: &mut Harness) -> Vec<(Vec<u8>, bool)> {
    let mut out = vec![];
    while let Some(buffer) = h.try_pull() {
        let map = buffer.map_readable().unwrap();
        let packet = rtp_types::RtpPacket::parse(&map).unwrap();
        out.push((packet.payload().to_vec(), packet.marker_bit()));
    }
    out
}

fn types(pkts: &[(Vec<u8>, bool)]) -> Vec<u8> {
    pkts.iter().map(|p| nal_type(p.0[1])).collect()
}

#[test]
fn test_single_nal() {
    let mut h = make_harness(1400, AggregateMode::None);
    let trail = nal(NAL_TRAIL, 0, 1, 16);
    push_au(&mut h, &au(&[&trail]), ClockTime::ZERO, false);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(pkts.len(), 1, "one single-NAL packet expected");
    assert_eq!(pkts[0].0, trail, "payload == the NAL bytes");
    assert!(pkts[0].1, "marker on the last (only) packet of the AU");
}

#[test]
fn test_aggregate_none_emits_singles() {
    let mut h = make_harness(1400, AggregateMode::None);
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    push_au(
        &mut h,
        &au(&[&vps, &sps, &pps, &idr]),
        ClockTime::ZERO,
        true,
    );
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(
        types(&pkts),
        vec![NAL_VPS, NAL_SPS, NAL_PPS, NAL_IDR_W_RADL]
    );
    assert!(pkts.last().unwrap().1, "marker on last packet");
}

#[test]
fn test_aggregate_zero_latency_bundles_non_vcl() {
    let mut h = make_harness(1400, AggregateMode::ZeroLatency);
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let aps = nal(NAL_PREFIX_APS, 0, 1, 10);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    push_au(
        &mut h,
        &au(&[&vps, &sps, &pps, &aps, &idr]),
        ClockTime::ZERO,
        true,
    );
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(types(&pkts), vec![RTP_TYPE_AP, NAL_IDR_W_RADL]);

    let ap = &pkts[0].0;
    let mut off = 2;
    for expected in [&vps, &sps, &pps, &aps] {
        let size = ((ap[off] as usize) << 8) | ap[off + 1] as usize;
        off += 2;
        assert_eq!(&ap[off..off + size], expected.as_slice());
        off += size;
    }
    assert_eq!(off, ap.len(), "AP fully consumed");
    assert!(pkts[1].1, "marker on the last packet");
}

#[test]
fn test_aggregate_max_bundles_vcl_too() {
    let mut h = make_harness(1400, AggregateMode::Max);
    let sps = nal(NAL_SPS, 0, 1, 10);
    let pps = nal(NAL_PPS, 0, 1, 8);
    let trail = nal(NAL_TRAIL, 0, 1, 20);
    push_au(&mut h, &au(&[&sps, &pps, &trail]), ClockTime::ZERO, false);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(types(&pkts), vec![RTP_TYPE_AP], "Max bundles VCL too");
    assert!(pkts[0].1, "marker on the (single) AP");
}

#[test]
fn test_ap_header_lowest_layer_tid() {
    let mut h = make_harness(1400, AggregateMode::ZeroLatency);
    let vps = nal(NAL_VPS, 2, 3, 8);
    let sps = nal(NAL_SPS, 4, 1, 8);
    let pps = nal(NAL_PPS, 0, 2, 8);
    let idr = nal(NAL_IDR_W_RADL, 5, 4, 20);
    push_au(
        &mut h,
        &au(&[&vps, &sps, &pps, &idr]),
        ClockTime::ZERO,
        true,
    );
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    let ap = &pkts[0].0;
    assert_eq!(nal_type(ap[1]), RTP_TYPE_AP);
    assert_eq!(ap[0] & 0x3f, 0, "LayerId = lowest of units (0)");
    assert_eq!(ap[1] & 0x7, 1, "TID = lowest of units (1)");
    assert_eq!(ap[0] & 0x80, 0, "F bit clear");
}

#[test]
fn test_fragmentation_units() {
    let mtu = 100u32;
    let mut h = make_harness(mtu, AggregateMode::None);
    let big = nal(NAL_TRAIL, 0, 1, 1000);
    push_au(&mut h, &au(&[&big]), ClockTime::ZERO, false);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert!(pkts.len() > 1, "large NAL fragmented");

    for (i, (payload, marker)) in pkts.iter().enumerate() {
        assert_eq!(nal_type(payload[1]), RTP_TYPE_FU, "every packet is an FU");
        let s = payload[2] & 0x80 != 0;
        let e = payload[2] & 0x40 != 0;
        assert_eq!(s, i == 0, "S only on first fragment");
        assert_eq!(e, i == pkts.len() - 1, "E only on last fragment");
        assert_eq!(fu_type(payload[2]), NAL_TRAIL, "FU type == original");
        assert_eq!(*marker, i == pkts.len() - 1, "marker only on last");
    }

    let mut rebuilt = vec![big[0], big[1]];
    for (payload, _) in &pkts {
        rebuilt.extend_from_slice(&payload[3..]);
    }
    assert_eq!(rebuilt, big, "FU reassembly is byte-exact");
}

#[test]
fn test_aud_and_filler_dropped() {
    let mut h = make_harness(1400, AggregateMode::None);
    let aud = nal(NAL_AUD, 0, 1, 4);
    let trail = nal(NAL_TRAIL, 0, 1, 8);
    let fd = nal(NAL_FD, 0, 1, 10);
    push_au(&mut h, &au(&[&aud, &trail, &fd]), ClockTime::ZERO, false);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(pkts.len(), 1, "only the TRAIL NAL is emitted");
    assert_eq!(pkts[0].0, trail);
}

#[test]
fn test_config_interval_reinserts_param_sets() {
    let mut h = make_harness(1400, AggregateMode::ZeroLatency);
    h.element().unwrap().set_property("config-interval", -1i32);

    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    let trail = nal(NAL_TRAIL, 0, 1, 30);

    push_au(
        &mut h,
        &au(&[&vps, &sps, &pps, &idr]),
        ClockTime::ZERO,
        true,
    );
    push_au(&mut h, &au(&[&trail]), ClockTime::from_seconds(1), false);
    push_au(&mut h, &au(&[&idr]), ClockTime::from_seconds(2), true);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(
        types(&pkts),
        vec![
            RTP_TYPE_AP,
            NAL_IDR_W_RADL,
            NAL_TRAIL,
            RTP_TYPE_AP,
            NAL_IDR_W_RADL
        ]
    );
}

#[test]
fn test_config_interval_disabled_by_default() {
    let mut h = make_harness(1400, AggregateMode::ZeroLatency);
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);

    push_au(
        &mut h,
        &au(&[&vps, &sps, &pps, &idr]),
        ClockTime::ZERO,
        true,
    );
    push_au(&mut h, &au(&[&idr]), ClockTime::from_seconds(1), true);
    h.push_event(Eos::new());

    let pkts = pull_packets(&mut h);
    assert_eq!(
        types(&pkts),
        vec![RTP_TYPE_AP, NAL_IDR_W_RADL, NAL_IDR_W_RADL]
    );
}

#[test]
fn test_sprop_caps_advertised() {
    let mut h = make_harness(1400, AggregateMode::None);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    push_au(&mut h, &au(&[&sps, &pps, &idr]), ClockTime::ZERO, true);
    h.push_event(Eos::new());
    let _ = pull_packets(&mut h);

    let caps = h
        .element()
        .unwrap()
        .static_pad("src")
        .unwrap()
        .current_caps()
        .expect("src caps set");
    let s = caps.structure(0).unwrap();
    let sprop_sps = s.get::<&str>("sprop-sps").expect("sprop-sps present");
    assert_eq!(glib::base64_decode(sprop_sps), sps);
    let sprop_pps = s.get::<&str>("sprop-pps").expect("sprop-pps present");
    assert_eq!(glib::base64_decode(sprop_pps), pps);
}

#[test]
fn test_property_roundtrip() {
    init();
    let h = Harness::new("rtph266pay");
    let pay = h.element().unwrap();
    assert_eq!(pay.property::<i32>("config-interval"), 0);
    pay.set_property("config-interval", 2i32);
    assert_eq!(pay.property::<i32>("config-interval"), 2);

    assert_eq!(
        pay.property::<AggregateMode>("aggregate-mode"),
        AggregateMode::None
    );
    pay.set_property("aggregate-mode", AggregateMode::Max);
    assert_eq!(
        pay.property::<AggregateMode>("aggregate-mode"),
        AggregateMode::Max
    );
}
