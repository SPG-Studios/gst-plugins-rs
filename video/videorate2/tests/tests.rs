// Copyright (C) 2025 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstvideorate2::plugin_register_static().unwrap();
    });
}

fn prepare_harness() -> gst_check::Harness {
    init();
    gst_check::Harness::new("videorate2")
}

fn prepare_caps(h: &mut gst_check::Harness, in_rate: i32, out_rate: i32) {
    h.set_src_caps(
        gst::Caps::builder("video/x-raw")
            .field("width", 320i32)
            .field("height", 240i32)
            .field("framerate", gst::Fraction::new(in_rate, 1))
            .build(),
    );

    h.set_sink_caps(
        gst::Caps::builder("video/x-raw")
            .field("width", 320i32)
            .field("height", 240i32)
            .field("framerate", gst::Fraction::new(out_rate, 1))
            .build(),
    );
}

fn unprepare_harness(mut h: gst_check::Harness, inc: u64, out: u64, duplicate: u64, drop: u64) {
    assert_eq!(h.buffers_in_queue(), 0);
    let e = h.element().unwrap();
    assert_eq!(e.property::<u64>("in"), inc);
    assert_eq!(e.property::<u64>("out"), out);
    assert_eq!(e.property::<u64>("duplicate"), duplicate);
    assert_eq!(e.property::<u64>("drop"), drop);
    h.push_event(gst::event::Eos::new());
}

fn prepare_buffer(
    index: u64,
    pts: gst::ClockTime,
    duration: Option<gst::ClockTime>,
) -> gst::Buffer {
    let mut buffer = gst::Buffer::with_size(1).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(pts);
        buffer.set_duration(duration);

        let mut map = buffer.map_writable().unwrap();
        let data = map.as_mut_slice();
        data[0] = index as u8;
    }

    buffer
}

fn check_buffer(
    mut buffer: gst::Buffer,
    index: Option<u64>,
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
) {
    let buffer = buffer.get_mut().unwrap();
    println!("expecting: {index:?} {pts:?} {duration:?}");

    if let Some(pts) = pts {
        println!("buffer PTS {:?}", buffer.pts());
        assert_eq!(buffer.pts(), Some(pts));
    }

    if let Some(duration) = duration {
        println!("buffer duration {:?}", buffer.duration());
        assert_eq!(buffer.duration(), Some(duration));
    }

    if let Some(index) = index {
        let mut map = buffer.map_writable().unwrap();
        let data = map.as_mut_slice();
        println!("buffer index {:?}", data[0]);
        assert_eq!(data[0], index as u8);
    }
}

#[test]
fn test_passthrough() {
    let mut h = prepare_harness();
    prepare_caps(&mut h, 1, 1);
    h.play();

    for i in 0..13 {
        let buffer = prepare_buffer(i, i.seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(i), Some(i.seconds()), Some(1.seconds()));
    }

    unprepare_harness(h, 13, 13, 0, 0);
}

fn test_fwd_downsample(new_pref: f64, drop_only: bool) {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", new_pref);
    h.element().unwrap().set_property("drop-only", drop_only);
    prepare_caps(&mut h, 4, 1);
    h.play();

    for i in 0..13 {
        let buffer = prepare_buffer(i, (i * 250).mseconds(), Some(250.mseconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 0..4 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(i * 4), Some(i.seconds()), Some(1.seconds()));
    }

    unprepare_harness(h, 13, 4, 0, 9);
}

#[test]
fn test_fwd_downsample_new_pref_0_0() {
    test_fwd_downsample(0.0, false);
}

#[test]
fn test_fwd_downsample_new_pref_0_5() {
    test_fwd_downsample(0.5, false);
}

#[test]
fn test_fwd_downsample_new_pref_1_0() {
    test_fwd_downsample(1.0, false);
}

#[test]
fn test_fwd_downsample_new_pref_0_5_drop_only() {
    test_fwd_downsample(0.5, true);
}

fn test_fwd_upsample(new_pref: f64, offset: u64, drop_only: bool) {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", new_pref);
    h.element().unwrap().set_property("drop-only", drop_only);
    prepare_caps(&mut h, 1, 4);
    h.play();

    for i in 0..4 {
        let buffer = prepare_buffer(i, i.seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 0..13 {
        if drop_only && i != 0 && !(i + offset).is_multiple_of(4) {
            continue;
        }
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + offset) / 4),
            Some((i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    if drop_only {
        unprepare_harness(h, 4, 4, 0, 0);
    } else {
        unprepare_harness(h, 4, 13, 9, 0);
    }
}

#[test]
fn test_fwd_upsample_0_0() {
    test_fwd_upsample(0.0, 0, false);
}

#[test]
fn test_fwd_upsample_0_5() {
    test_fwd_upsample(0.5, 2, false);
}

#[test]
fn test_fwd_upsample_1_0() {
    test_fwd_upsample(1.0, 3, false);
}

#[test]
fn test_fwd_upsample_0_0_drop_only() {
    test_fwd_upsample(0.0, 0, true);
}

#[test]
fn test_fwd_upsample_0_5_drop_only() {
    test_fwd_upsample(0.5, 2, true);
}

#[test]
fn test_fwd_upsample_skip_to_first() {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", 0.5);
    h.element().unwrap().set_property("skip-to-first", true);
    prepare_caps(&mut h, 1, 4);
    h.play();

    for i in 0..4 {
        let buffer = prepare_buffer(i, 1.seconds() + i.seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + 2) / 4),
            Some(1.seconds() + (i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    unprepare_harness(h, 4, 13, 9, 0);
}

#[test]
fn test_fwd_upsample_no_skip_to_first() {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", 0.5);
    h.element().unwrap().set_property("skip-to-first", false);
    prepare_caps(&mut h, 1, 4);
    h.play();

    for i in 0..4 {
        let buffer = prepare_buffer(i, 1.seconds() + i.seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    // 4 buffers that fill the gap to the first buffer PTS
    for i in 0..4 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some(0),
            Some((i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + 2) / 4),
            Some(1.seconds() + (i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    unprepare_harness(h, 4, 17, 13, 0);
}

fn test_rev_upsample(new_pref: f64, offset: u64) {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", new_pref);
    prepare_caps(&mut h, 1, 4);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.do_seek(
        -1.0,
        gst::SeekFlags::TRICKMODE,
        gst::SeekType::Set,
        0.seconds(),
        gst::SeekType::Set,
        4.seconds(),
    );
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    h.play();

    let mut ts = 4.seconds() - 1.seconds();
    for i in 0..4 {
        let buffer = prepare_buffer(i, ts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        ts = ts.saturating_sub(1.seconds());
    }

    ts = 4.seconds() - 250.mseconds();
    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + offset) / 4),
            Some(ts),
            Some(250.mseconds()),
        );
        ts = ts.saturating_sub(250.mseconds());
    }

    unprepare_harness(h, 4, 13, 9, 0);
}

#[test]
fn test_rev_upsample_0_0() {
    test_rev_upsample(0.0, 0);
}

#[test]
fn test_rev_upsample_0_5() {
    test_rev_upsample(0.5, 2);
}

#[test]
fn test_rev_upsample_1_0() {
    test_rev_upsample(1.0, 3);
}

fn test_rev_downsample(new_pref: f64) {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", new_pref);
    prepare_caps(&mut h, 4, 1);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.do_seek(
        -1.0,
        gst::SeekFlags::TRICKMODE,
        gst::SeekType::Set,
        0.seconds(),
        gst::SeekType::Set,
        4.seconds(),
    );
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    h.play();

    let mut pts = 4.seconds() - 250.mseconds();
    for i in 0..13 {
        let buffer = prepare_buffer(i, pts, Some(250.mseconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts = pts.saturating_sub(250.mseconds());
    }

    pts = 4.seconds() - 1.seconds();
    for i in 0..4 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(i * 4), Some(pts), Some(1.seconds()));
        pts = pts.saturating_sub(1.seconds());
    }

    unprepare_harness(h, 13, 4, 0, 9);
}

#[test]
fn test_rev_downsample_new_pref_0_0() {
    test_rev_downsample(0.0);
}

#[test]
fn test_rev_downsample_new_pref_0_5() {
    test_rev_downsample(0.5);
}

#[test]
fn test_rev_downsample_new_pref_1_0() {
    test_rev_downsample(1.0);
}

fn test_fwd_upsample_src_caps_change(new_pref: f64, offset: u64, post_offset: u64) {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", new_pref);
    prepare_caps(&mut h, 1, 4);
    h.play();

    let mut pts = 0.seconds();
    for i in 0..4 {
        let buffer = prepare_buffer(i, pts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts += 1.seconds();
    }

    h.set_src_caps(
        gst::Caps::builder("video/x-raw")
            .field("width", 320i32)
            .field("height", 240i32)
            .field("framerate", gst::Fraction::new(2, 1))
            .build(),
    );

    for i in 4..8 {
        let buffer = prepare_buffer(i, pts, Some(500.mseconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts += 500.mseconds();
    }

    pts = 0.seconds();
    // Buffers from first segment
    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + offset) / 4),
            Some(pts),
            Some(250.mseconds()),
        );
        pts += 250.mseconds();
    }

    // Buffers from transition from first to second segment
    for i in 13..16 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i + offset) / 4),
            Some(pts),
            Some(250.mseconds()),
        );
        pts += 250.mseconds();
    }

    // Buffers second segment
    for i in 16..23 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some((i - post_offset) / 2),
            Some(pts),
            Some(250.mseconds()),
        );
        pts += 250.mseconds();
    }

    unprepare_harness(h, 8, 23, 15, 0);
}

#[test]
fn test_fwd_upsample_src_caps_change_0_0() {
    test_fwd_upsample_src_caps_change(0.0, 0, 8);
}

#[test]
fn test_fwd_upsample_src_caps_change_0_5() {
    test_fwd_upsample_src_caps_change(0.5, 2, 7);
}

#[test]
fn test_fwd_upsample_src_caps_change_1_0() {
    test_fwd_upsample_src_caps_change(1.0, 3, 7);
}

#[test]
fn test_segments_fwd_gap() {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", 0.5);
    // This must be at least 2s to duplicate until the next segment is reached
    h.element()
        .unwrap()
        .set_property("max-closing-segment-duplication-duration", 2_000_000_000u64);
    prepare_caps(&mut h, 1, 4);
    h.play();

    let mut pts = 0.seconds();
    for i in 0..4 {
        let buffer = prepare_buffer(i, pts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts += 1.seconds();
    }

    // Seek from position 1s to position 5s.
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.do_seek(
        1.0,
        gst::SeekFlags::TRICKMODE,
        gst::SeekType::Set,
        5.seconds(),
        gst::SeekType::Set,
        10.seconds(),
    );
    assert!(h.push_event(gst::event::Segment::new(&segment)));

    pts = 6.seconds();
    for i in 4..8 {
        let buffer = prepare_buffer(i, pts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts += 1.seconds();
    }

    // The first four buffers upsampled.
    pts = 0.seconds();
    for i in 0..14 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some((i + 2) / 4), Some(pts), Some(250.mseconds()));
        pts += 250.mseconds();
    }

    // Another 6 times the last buffer to fill the gap to the next
    // segment 3.5s -> 5s. Offset comes with
    // max_segment_closing_duration, if this is less than 2s
    // duplication will stop early.
    for _ in 14..20 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(3), Some(pts), Some(250.mseconds()));
        pts += 250.mseconds();
    }

    // 6-times the new buffer to fill the gap from segment start (5s) to
    // buffer PTS + 0.25 (because new_pref == 0.5) (6.25s).
    for _ in 20..26 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(4), Some(pts), Some(250.mseconds()));
        pts += 250.mseconds();
    }

    for i in 26..37 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some((i - 6) / 4), Some(pts), Some(250.mseconds()));
        pts += 250.mseconds();
    }

    unprepare_harness(h, 8, 37, 29, 0);
}

#[test]
fn test_segments_rev_gap() {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", 0.5);
    prepare_caps(&mut h, 1, 4);
    h.play();

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.do_seek(
        -1.0,
        gst::SeekFlags::TRICKMODE,
        gst::SeekType::Set,
        12.seconds(),
        gst::SeekType::Set,
        16.seconds(),
    );
    assert!(h.push_event(gst::event::Segment::new(&segment)));
    let mut pts = 16.seconds() - 1.seconds();
    for i in 0..4 {
        let buffer = prepare_buffer(i, pts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts -= 1.seconds();
    }

    // Seek from position 1s to position 5s.
    segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.do_seek(
        -1.0,
        gst::SeekFlags::TRICKMODE,
        gst::SeekType::Set,
        6.seconds(),
        gst::SeekType::Set,
        10.seconds(),
    );
    assert!(h.push_event(gst::event::Segment::new(&segment)));

    pts = 10.seconds() - 1.seconds();
    for i in 4..8 {
        let buffer = prepare_buffer(i, pts, Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
        pts -= 1.seconds();
    }

    // The first four buffers upsampled.
    pts = 15750.mseconds();
    for i in 0..14 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some((i + 2) / 4), Some(pts), Some(250.mseconds()));
        pts -= 250.mseconds();
    }

    // Another 2 times the last buffer to fill the segment.
    for _ in 14..16 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(3), Some(pts), Some(250.mseconds()));
        pts -= 250.mseconds();
    }

    // Adjust PTS to next segment start.
    pts = 9750.mseconds();
    for _ in 16..18 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some(4), Some(pts), Some(250.mseconds()));
        pts -= 250.mseconds();
    }

    for i in 18..29 {
        let buffer = h.pull().unwrap();
        check_buffer(buffer, Some((i + 2) / 4), Some(pts), Some(250.mseconds()));
        pts -= 250.mseconds();
    }

    unprepare_harness(h, 8, 29, 21, 0);
}

#[test]
fn test_fwd_upsample_max_dup_time() {
    let mut h = prepare_harness();
    h.element().unwrap().set_property("new-pref", 0.0);
    h.element()
        .unwrap()
        .set_property("max-duplication-time", 1_000_000_000u64);
    prepare_caps(&mut h, 1, 4);
    h.play();

    for i in 0..2 {
        let buffer = prepare_buffer(i, i.seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 2..4 {
        let buffer = prepare_buffer(i, (i + 1).seconds(), Some(1.seconds()));
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    // From the 1st buffer we get 4 output buffers, the 2nd buffer is
    // output once.
    for i in 0..5 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some(i / 4),
            Some((i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    // No further buffer duplication of the 2nd buffer because the gap
    // to the 3rd buffer exceeds the max-duplication-time. So next
    // buffer is the 3rd buffer repeated 4 times and then the 4th
    // buffer.
    for i in 0..5 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some(2 + i / 4),
            Some((3_000 + i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    unprepare_harness(h, 4, 10, 6, 0);
}

#[test]
fn test_fwd_resample() {
    init();

    let mut h = gst_check::Harness::new("videorate2");
    h.set_src_caps(
        gst::Caps::builder("video/x-raw")
            .field("width", 320i32)
            .field("height", 240i32)
            .build(),
    );

    h.set_sink_caps(
        gst::Caps::builder("video/x-raw")
            .field("width", 320i32)
            .field("height", 240i32)
            .field("framerate", gst::Fraction::new(4, 1))
            .build(),
    );

    h.element().unwrap().set_property("new-pref", 0.0);
    h.element().unwrap().set_property("drop-only", false);
    h.play();

    for i in 0..4 {
        let buffer = prepare_buffer(i, i.seconds(), None);
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    for i in 0..13 {
        let buffer = h.pull().unwrap();
        check_buffer(
            buffer,
            Some(i / 4),
            Some((i * 250).mseconds()),
            Some(250.mseconds()),
        );
    }

    unprepare_harness(h, 4, 13, 9, 0);
}
