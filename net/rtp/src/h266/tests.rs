//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! End-to-end round-trip tests: rtph266pay -> rtph266depay.
//!
//! These complement the isolated payloader/depayloader tests in `pay/tests.rs`
//! and `depay/tests.rs` by feeding synthetic H266 access units through the full
//! payloader -> depayloader chain (via the shared [`crate::tests`] harness) and
//! asserting that the depayloaded access units are byte-exact. They exercise the
//! settings that affect pay/depay interoperability: every aggregate-mode,
//! Fragmentation Units, both `config-interval` policies (every-keyframe and the
//! time-based variant), and packet-loss recovery with `wait-for-keyframe`.

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline_and_validate_data};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtph266 round-trip test");
    });
}

// H266 NAL unit types (ITU-T H.266 Table 5).
const NAL_TRAIL: u8 = 1;
const NAL_IDR_W_RADL: u8 = 7;
const NAL_OPI: u8 = 12;
const NAL_DCI: u8 = 13;
const NAL_VPS: u8 = 14;
const NAL_SPS: u8 = 15;
const NAL_PPS: u8 = 16;
const NAL_PREFIX_APS: u8 = 17;
const NAL_SUFFIX_APS: u8 = 18;
const NAL_PH: u8 = 19;
const NAL_AUD: u8 = 20;
const NAL_PREFIX_SEI: u8 = 23;

/// Build a synthetic NAL unit of `len` bytes with a well-formed 2-byte header
/// and deterministic payload.
fn nal(ty: u8, layer: u8, tid: u8, len: usize) -> Vec<u8> {
    assert!(len >= 2);
    let mut n = vec![layer & 0x3f, ((ty & 0x1f) << 3) | (tid & 0x7)];
    for i in 0..len - 2 {
        n.push((i % 251) as u8);
    }
    n
}

/// Concatenate NAL units into an Annex-B access unit (4-byte start codes).
fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![];
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

fn h266_caps() -> gst::Caps {
    gst::Caps::builder("video/x-h266")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build()
}

/// One input access unit. `discont` should be set on the first buffer only.
fn input_au(au: Vec<u8>, pts: gst::ClockTime, discont: bool) -> gst::Buffer {
    init();
    let mut buf = gst::Buffer::from_mut_slice(au);
    {
        let r = buf.get_mut().unwrap();
        r.set_pts(pts);
        if discont {
            r.set_flags(gst::BufferFlags::DISCONT);
        }
    }
    buf
}

/// One expected RTP packet (payload type is always the default 96).
fn pkt(
    pts: gst::ClockTime,
    flags: gst::BufferFlags,
    marker: bool,
    rtp_time: u32,
    size: usize,
    drop: bool,
) -> ExpectedPacket {
    ExpectedPacket::builder()
        .pts(pts)
        .flags(flags)
        .pt(96)
        .rtp_time(rtp_time)
        .marker_bit(marker)
        .size(size)
        .drop(drop)
        .build()
}

/// One expected depayloaded access-unit buffer (size is checked via the
/// byte-exact validator instead).
fn buf(pts: gst::ClockTime, flags: gst::BufferFlags) -> ExpectedBuffer {
    ExpectedBuffer::builder().pts(pts).flags(flags).build()
}

/// Run a pay -> depay round trip and assert the depayloaded access units are
/// byte-exact against `expected_aus` (a `None` entry skips the byte check for
/// that access unit, e.g. when the depayloader legitimately prepends the
/// out-of-band parameter sets to the first emitted AU).
fn run_roundtrip(
    inputs: Vec<gst::Buffer>,
    pay: &str,
    depay: &str,
    expected_pay: Vec<Vec<ExpectedPacket>>,
    expected_depay: Vec<Vec<ExpectedBuffer>>,
    expected_aus: Vec<Option<Vec<u8>>>,
) {
    init();
    run_test_pipeline_and_validate_data(
        Source::Buffers(h266_caps(), inputs),
        pay,
        depay,
        expected_pay,
        expected_depay,
        move |data, i, _j| {
            if let Some(Some(expected)) = expected_aus.get(i)
                && data != expected.as_slice()
            {
                anyhow::bail!(
                    "AU {i}: depayloaded {} bytes, expected {} bytes (byte mismatch)",
                    data.len(),
                    expected.len(),
                );
            }
            Ok(())
        },
    );
}

const DISCONT: gst::BufferFlags = gst::BufferFlags::DISCONT;
const MARKER: gst::BufferFlags = gst::BufferFlags::MARKER;
const DELTA: gst::BufferFlags = gst::BufferFlags::DELTA_UNIT;

// One keyframe AU with two non-VCL NALs (prefix/suffix APS — deliberately not
// parameter sets, so the payloader does not advertise sprop-* and the
// depayloader output is a clean reconstruction) followed by an IDR. Reused by
// the three aggregate-mode round trips, which must all reconstruct the same AU.
fn aggregate_au() -> (Vec<u8>, Vec<u8>) {
    let apsa = nal(NAL_PREFIX_APS, 0, 1, 10);
    let apsb = nal(NAL_SUFFIX_APS, 0, 1, 8);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    let au = annexb(&[&apsa, &apsb, &idr]);
    (au.clone(), au)
}

#[test]
fn roundtrip_aggregate_none() {
    let (au, depay_au) = aggregate_au();
    let inputs = vec![input_au(au, gst::ClockTime::ZERO, true)];

    // None: every NAL in its own packet -> [APS_a, APS_b, IDR].
    let expected_pay = vec![vec![
        pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 12 + 10, false),
        pkt(
            gst::ClockTime::ZERO,
            gst::BufferFlags::empty(),
            false,
            0,
            12 + 8,
            false,
        ),
        pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 40, false),
    ]];
    let expected_depay = vec![vec![buf(gst::ClockTime::ZERO, DISCONT)]];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=none",
        "rtph266depay",
        expected_pay,
        expected_depay,
        vec![Some(depay_au)],
    );
}

#[test]
fn roundtrip_aggregate_zero_latency() {
    let (au, depay_au) = aggregate_au();
    let inputs = vec![input_au(au, gst::ClockTime::ZERO, true)];

    // ZeroLatency: bundle the two non-VCL NALs into one AP, then the IDR alone.
    // AP payload = 2 + (2+10) + (2+8) = 24 -> packet 12+24 = 36.
    let expected_pay = vec![vec![
        pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 36, false),
        pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 40, false),
    ]];
    let expected_depay = vec![vec![buf(gst::ClockTime::ZERO, DISCONT)]];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=zero-latency",
        "rtph266depay",
        expected_pay,
        expected_depay,
        vec![Some(depay_au)],
    );
}

#[test]
fn roundtrip_aggregate_max() {
    let (au, depay_au) = aggregate_au();
    let inputs = vec![input_au(au, gst::ClockTime::ZERO, true)];

    // Max: all three NALs fit one AP.
    // AP payload = 2 + (2+10) + (2+8) + (2+40) = 66 -> packet 12+66 = 78.
    let expected_pay = vec![vec![pkt(
        gst::ClockTime::ZERO,
        DISCONT | MARKER,
        true,
        0,
        78,
        false,
    )]];
    let expected_depay = vec![vec![buf(gst::ClockTime::ZERO, DISCONT)]];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=max",
        "rtph266depay",
        expected_pay,
        expected_depay,
        vec![Some(depay_au)],
    );
}

#[test]
fn roundtrip_fu_fragmentation() {
    // A single large IDR with a small MTU forces Fragmentation Units, which the
    // depayloader must reassemble byte-exact.
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 200);
    let au = annexb(&[&idr]);
    let inputs = vec![input_au(au, gst::ClockTime::ZERO, true)];

    // max_payload = mtu(100) - 12 = 88; FU overhead = 3 -> 85 bytes/fragment.
    // NAL payload = 200 - 2 = 198 -> fragments of 85, 85, 28.
    let expected_pay = vec![vec![
        pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 12 + 3 + 85, false),
        pkt(
            gst::ClockTime::ZERO,
            gst::BufferFlags::empty(),
            false,
            0,
            12 + 3 + 85,
            false,
        ),
        pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 3 + 28, false),
    ]];
    let expected_depay = vec![vec![buf(gst::ClockTime::ZERO, DISCONT)]];

    run_roundtrip(
        inputs,
        "rtph266pay mtu=100 aggregate-mode=none",
        "rtph266depay",
        expected_pay,
        expected_depay,
        vec![Some(annexb(&[&idr]))],
    );
}

#[test]
fn roundtrip_config_interval_minus1() {
    // config-interval=-1 re-inserts VPS/SPS/PPS ahead of every keyframe AU.
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    let trail = nal(NAL_TRAIL, 0, 1, 16);

    let inputs = vec![
        input_au(
            annexb(&[&vps, &sps, &pps, &idr]),
            gst::ClockTime::ZERO,
            true,
        ),
        input_au(annexb(&[&trail]), gst::ClockTime::from_seconds(1), false),
        input_au(annexb(&[&idr]), gst::ClockTime::from_seconds(2), false),
    ];

    // AP(VPS,SPS,PPS) payload = 2 + (2+8) + (2+12) + (2+6) = 34 -> packet 46.
    let s1 = gst::ClockTime::from_seconds(1);
    let s2 = gst::ClockTime::from_seconds(2);
    let expected_pay = vec![
        // AU0: params in-stream -> AP + IDR.
        vec![
            pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 46, false),
            pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 40, false),
        ],
        // AU1: delta TRAIL, single packet.
        vec![pkt(s1, MARKER, true, 90_000, 12 + 16, false)],
        // AU2: keyframe without in-stream params -> params re-inserted as AP + IDR.
        vec![
            pkt(s2, gst::BufferFlags::empty(), false, 180_000, 46, false),
            pkt(s2, MARKER, true, 180_000, 12 + 40, false),
        ],
    ];

    let expected_depay = vec![
        vec![buf(gst::ClockTime::ZERO, DISCONT)],
        vec![buf(s1, DELTA)],
        vec![buf(s2, gst::BufferFlags::empty())],
    ];

    // AU0's exact bytes depend on the out-of-band sprop prepend, so skip it; the
    // point of this test is AU2 carrying the re-inserted parameter sets.
    let expected_aus = vec![
        None,
        Some(annexb(&[&trail])),
        Some(annexb(&[&vps, &sps, &pps, &idr])),
    ];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=zero-latency config-interval=-1",
        "rtph266depay",
        expected_pay,
        expected_depay,
        expected_aus,
    );
}

#[test]
fn roundtrip_config_interval_seconds() {
    // config-interval=1 re-inserts parameter sets ahead of a keyframe only once
    // at least one second has elapsed since the last insertion. Keyframes at
    // 0s (in-stream), 0.5s (too soon -> no re-insert) and 2s (re-inserted).
    let vps = nal(NAL_VPS, 0, 1, 8);
    let sps = nal(NAL_SPS, 0, 1, 12);
    let pps = nal(NAL_PPS, 0, 1, 6);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);

    let half = gst::ClockTime::from_mseconds(500);
    let s2 = gst::ClockTime::from_seconds(2);
    let inputs = vec![
        input_au(
            annexb(&[&vps, &sps, &pps, &idr]),
            gst::ClockTime::ZERO,
            true,
        ),
        input_au(annexb(&[&idr]), half, false),
        input_au(annexb(&[&idr]), s2, false),
    ];

    let expected_pay = vec![
        // AU0: params in-stream -> AP + IDR.
        vec![
            pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 46, false),
            pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 40, false),
        ],
        // AU1 @ 0.5s: within the interval -> no re-insert, IDR alone.
        vec![pkt(half, MARKER, true, 45_000, 12 + 40, false)],
        // AU2 @ 2s: interval elapsed -> params re-inserted as AP + IDR.
        vec![
            pkt(s2, gst::BufferFlags::empty(), false, 180_000, 46, false),
            pkt(s2, MARKER, true, 180_000, 12 + 40, false),
        ],
    ];

    let expected_depay = vec![
        vec![buf(gst::ClockTime::ZERO, DISCONT)],
        vec![buf(half, gst::BufferFlags::empty())],
        vec![buf(s2, gst::BufferFlags::empty())],
    ];

    // AU1 must NOT carry parameter sets (still within the interval); AU2 must.
    let expected_aus = vec![
        None,
        Some(annexb(&[&idr])),
        Some(annexb(&[&vps, &sps, &pps, &idr])),
    ];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=zero-latency config-interval=1",
        "rtph266depay",
        expected_pay,
        expected_depay,
        expected_aus,
    );
}

#[test]
fn roundtrip_packet_loss_recovery() {
    // Drop a middle fragment of the keyframe AU. With wait-for-keyframe the
    // depayloader must discard the corrupted keyframe AU and the following delta
    // AU, then recover on the next intact keyframe.
    let big_idr = nal(NAL_IDR_W_RADL, 0, 1, 200);
    let trail = nal(NAL_TRAIL, 0, 1, 16);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 16);

    let s1 = gst::ClockTime::from_seconds(1);
    let s2 = gst::ClockTime::from_seconds(2);
    let inputs = vec![
        input_au(annexb(&[&big_idr]), gst::ClockTime::ZERO, true),
        input_au(annexb(&[&trail]), s1, false),
        input_au(annexb(&[&idr]), s2, false),
    ];

    let expected_pay = vec![
        // AU0: three FUs; drop the middle one.
        vec![
            pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 12 + 3 + 85, false),
            pkt(
                gst::ClockTime::ZERO,
                gst::BufferFlags::empty(),
                false,
                0,
                12 + 3 + 85,
                true,
            ),
            pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 3 + 28, false),
        ],
        // AU1: delta TRAIL (discarded by the depayloader while waiting).
        vec![pkt(s1, MARKER, true, 90_000, 12 + 16, false)],
        // AU2: intact keyframe -> recovery.
        vec![pkt(s2, MARKER, true, 180_000, 12 + 16, false)],
    ];

    // Only the recovered keyframe AU is emitted.
    let expected_depay = vec![vec![buf(s2, DISCONT)]];

    run_roundtrip(
        inputs,
        "rtph266pay mtu=100 aggregate-mode=none",
        "rtph266depay wait-for-keyframe=true",
        expected_pay,
        expected_depay,
        vec![Some(annexb(&[&idr]))],
    );
}

#[test]
fn roundtrip_non_vcl_aud_dropped() {
    // A realistic access unit with the assorted non-VCL NAL types. The AUD is
    // dropped over RTP (its job is done by the marker bit); OPI/DCI/PH/SEI are
    // ordinary non-VCL units that must be forwarded and reassembled in order.
    // No parameter sets, so the payloader does not advertise sprop-* and the
    // depayloaded AU is a clean, byte-exact reconstruction.
    let aud = nal(NAL_AUD, 0, 1, 6); // dropped
    let opi = nal(NAL_OPI, 0, 1, 8);
    let dci = nal(NAL_DCI, 0, 1, 6);
    let ph = nal(NAL_PH, 0, 1, 10);
    let sei = nal(NAL_PREFIX_SEI, 0, 1, 12);
    let idr = nal(NAL_IDR_W_RADL, 0, 1, 40);
    let inputs = vec![input_au(
        annexb(&[&aud, &opi, &dci, &ph, &sei, &idr]),
        gst::ClockTime::ZERO,
        true,
    )];

    // ZeroLatency: AUD dropped; the four non-VCL NALs bundle into one AP, then
    // the IDR alone. AP payload = 2 + (2+8) + (2+6) + (2+10) + (2+12) = 46 ->
    // packet 12+46 = 58.
    let expected_pay = vec![vec![
        pkt(gst::ClockTime::ZERO, DISCONT, false, 0, 58, false),
        pkt(gst::ClockTime::ZERO, MARKER, true, 0, 12 + 40, false),
    ]];
    let expected_depay = vec![vec![buf(gst::ClockTime::ZERO, DISCONT)]];

    // AUD must be absent; every other NAL preserved in its original order.
    let expected_aus = vec![Some(annexb(&[&opi, &dci, &ph, &sei, &idr]))];

    run_roundtrip(
        inputs,
        "rtph266pay aggregate-mode=zero-latency",
        "rtph266depay",
        expected_pay,
        expected_depay,
        expected_aus,
    );
}
