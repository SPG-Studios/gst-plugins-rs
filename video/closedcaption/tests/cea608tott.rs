// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst::ClockTime;

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

#[test]
fn test_parse() {
    init();
    let data = include_bytes!("dn2018-1217.scc").as_ref();

    let mut h = gst_check::Harness::new_parse("sccparse ! cea608tott");
    h.set_live(false);
    h.set_src_caps_str("application/x-scc");
    h.set_sink_caps_str("text/x-raw");

    let buf = gst::Buffer::from_mut_slice(Vec::from(data));
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    // Check the first 4 output buffers
    let expected: [(ClockTime, ClockTime, &'static str); 4] = [
        (
            15_048_366_666.nseconds(),
            3_236_566_667.nseconds(),
            "From New York,\r\nthis is Democracy Now!",
        ),
        (
            18_985_633_333.nseconds(),
            1_234_566_667.nseconds(),
            "Yes, I’m supporting\r\nDonald Trump.",
        ),
        (
            20_220_200_000.nseconds(),
            2_168_833_333.nseconds(),
            "I’m doing so as enthusiastically\r\nas I can,",
        ),
        (
            22_389_033_333.nseconds(),
            2_235_566_667.nseconds(),
            "even the fact I think\r\nhe’s a terrible human being.",
        ),
    ];

    for (i, e) in expected.iter().enumerate() {
        let buf = h.try_pull().unwrap();

        assert_eq!(
            e.0,
            buf.pts().unwrap(),
            "Unexpected PTS for {}th buffer",
            i + 1
        );
        assert_eq!(
            e.1,
            buf.duration().unwrap(),
            "Unexpected duration for {}th buffer",
            i + 1
        );

        let data = buf.map_readable().unwrap();
        let s = std::str::from_utf8(&data)
            .unwrap_or_else(|_| panic!("Non-UTF8 data for {}th buffer", i + 1));
        assert_eq!(e.2, s, "Unexpected data for {}th buffer", i + 1);
    }

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst::Caps::builder("text/x-raw")
            .field("format", "utf8")
            .build()
    );
}

fn new_timed_buffer<T: AsRef<[u8]> + Send + 'static>(
    slice: T,
    timestamp: ClockTime,
    duration: ClockTime,
) -> gst::buffer::Buffer {
    let mut buf = gst::Buffer::from_slice(slice);
    let buf_ref = buf.get_mut().unwrap();
    buf_ref.set_pts(timestamp);
    buf_ref.set_duration(duration);
    buf
}

#[test]
fn test_live() {
    init();
    let mut h = gst_check::Harness::new_parse("cea608tott");
    h.set_live(true);
    h.set_sink_caps_str("text/x-raw, format=utf8");
    h.set_src_caps_str("closedcaption/x-cea-608, format=raw");

    let input: [(ClockTime, ClockTime, [u8; 2usize]); 7] = [
        (
            1_000_000_000.nseconds(),
            33_333_333.nseconds(),
            [0x94, 0x20],
        ), /* resume_caption_loading */
        (
            1_033_333_333.nseconds(),
            33_333_334.nseconds(),
            [0x94, 0xae],
        ), /* erase_non_displayed_memory */
        (
            1_066_666_667.nseconds(),
            33_333_333.nseconds(),
            [0x94, 0x70],
        ), /* preamble */
        (
            1_100_000_000.nseconds(),
            33_333_333.nseconds(),
            [0xc8, 0xe5],
        ), /* H e */
        (
            1_133_333_333.nseconds(),
            33_333_334.nseconds(),
            [0xec, 0xec],
        ), /* l l */
        (
            1_166_666_667.nseconds(),
            33_333_333.nseconds(),
            [0xef, 0x80],
        ), /* o, nil */
        (
            1_200_000_000.nseconds(),
            33_333_333.nseconds(),
            [0x94, 0x2f],
        ), /* end_of_caption */
    ];

    for (pts, duration, data) in input {
        let buf = new_timed_buffer(data, pts, duration);

        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    }

    // When live, the element will output text as soon as it needs to be
    // displayed on screen, and will not assign a duration to it

    let buf = h.try_pull().unwrap();
    assert_eq!(
        buf.pts().unwrap(),
        1_200_000_000.nseconds(),
        "Unexpected PTS",
    );
    assert_eq!(buf.duration(), gst::ClockTime::NONE, "Unexpected duration",);

    let data = buf.map_readable().unwrap();
    let s = std::str::from_utf8(&data).unwrap_or_else(|_| panic!("Non-UTF8 data"));
    assert_eq!("Hello", s, "Unexpected data");
}
