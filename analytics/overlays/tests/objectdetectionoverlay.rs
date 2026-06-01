// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_analytics::AnalyticsRelationMetaODExt;

const WIDTH: usize = 64;
const HEIGHT: usize = 64;
const STRIDE: usize = WIDTH * 4;
const BUFFER_SIZE: usize = STRIDE * HEIGHT;

fn init() {
    use std::sync::Once;

    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

fn make_harness() -> gst_check::Harness {
    let mut harness = gst_check::Harness::new("odoverlay");
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness.set_sink_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);

    push_time_segment_to_harness(&mut harness);

    harness
}

fn push_time_segment_to_harness(harness: &mut gst_check::Harness) {
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::builder(&segment).build()));
}

fn make_buffer(pts: gst::ClockTime, with_meta: bool) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(pts);

        if with_meta {
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 10, 10, 24, 24, 0.9)
                .unwrap();
        }
    }

    buffer
}

fn make_oriented_buffer(pts: gst::ClockTime, rotation: f32) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(pts);

        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_oriented_od_mtd(glib::Quark::from_str("hand"), 20, 22, 28, 18, rotation, 0.9)
            .unwrap();
    }

    buffer
}

fn buffer_has_drawn_pixels(buffer: &gst::Buffer) -> bool {
    let map = buffer.map_readable().unwrap();
    map.as_slice().iter().any(|byte| *byte != 0)
}

fn make_pipeline() -> (gst::Pipeline, gst_app::AppSrc, gst_app::AppSink) {
    let pipeline = gst::Pipeline::new();
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", WIDTH as i32)
        .field("height", HEIGHT as i32)
        .field("framerate", gst::Fraction::new(1, 1))
        .build();
    let appsrc = gst_app::AppSrc::builder()
        .name("src")
        .caps(&caps)
        .format(gst::Format::Time)
        .build();
    let overlay = gst::ElementFactory::make("odoverlay")
        .property("render-enabled", true)
        .property("expire-overlay", gst::ClockTime::from_seconds(1).nseconds())
        .build()
        .unwrap();
    let appsink = gst_app::AppSink::builder()
        .name("sink")
        .caps(&caps)
        .sync(false)
        .build();

    pipeline
        .add_many([appsrc.upcast_ref(), &overlay, appsink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([appsrc.upcast_ref(), &overlay, appsink.upcast_ref()]).unwrap();

    (pipeline, appsrc, appsink)
}

fn pull_buffer_from_appsink(appsink: &gst_app::AppSink) -> gst::Buffer {
    appsink.pull_sample().unwrap().buffer().unwrap().copy()
}

fn wait_for_pipeline_eos(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();

    for message in bus.iter_timed(gst::ClockTime::NONE) {
        match message.view() {
            gst::MessageView::Eos(..) => break,
            gst::MessageView::Error(err) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                err.src().map(|src| src.path_string()),
                err.error(),
                err.debug()
            ),
            _ => {}
        }
    }
}

#[test]
fn expire_overlay_reuses_then_expires_end_to_end() {
    init();

    let mut harness = make_harness();
    harness
        .element()
        .unwrap()
        .set_property("expire-overlay", gst::ClockTime::from_seconds(1).nseconds());

    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::ZERO, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let first = harness.pull().unwrap();
    assert!(buffer_has_drawn_pixels(&first));

    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_mseconds(500), false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let reused = harness.pull().unwrap();
    assert!(buffer_has_drawn_pixels(&reused));

    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_seconds(2), false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let expired = harness.pull().unwrap();
    assert!(!buffer_has_drawn_pixels(&expired));
}

#[test]
fn eos_event_is_accepted() {
    init();

    let mut harness = make_harness();

    assert!(harness.push_event(gst::event::Eos::new()));
}

#[test]
fn flush_stop_restores_processing_after_flush_start() {
    init();

    let mut harness = make_harness();

    assert!(harness.push_event(gst::event::FlushStart::new()));
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::ZERO, true)),
        Err(gst::FlowError::Flushing)
    );

    assert!(harness.push_event(gst::event::FlushStop::new(true)));
    push_time_segment_to_harness(&mut harness);
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_mseconds(100), true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let buffer = harness.pull().unwrap();
    assert!(buffer_has_drawn_pixels(&buffer));
}

#[test]
fn pipeline_expire_overlay_reuses_then_expires() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline();
    pipeline.set_state(gst::State::Playing).unwrap();

    appsrc
        .push_buffer(make_buffer(gst::ClockTime::ZERO, true))
        .unwrap();
    appsrc
        .push_buffer(make_buffer(gst::ClockTime::from_mseconds(500), false))
        .unwrap();
    appsrc
        .push_buffer(make_buffer(gst::ClockTime::from_seconds(2), false))
        .unwrap();
    appsrc.end_of_stream().unwrap();

    let first = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&first));

    let reused = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&reused));

    let expired = pull_buffer_from_appsink(&appsink);
    assert!(!buffer_has_drawn_pixels(&expired));

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_eos_ends_stream() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline();
    pipeline.set_state(gst::State::Playing).unwrap();

    appsrc
        .push_buffer(make_buffer(gst::ClockTime::ZERO, true))
        .unwrap();
    appsrc.end_of_stream().unwrap();

    let first = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&first));

    wait_for_pipeline_eos(&pipeline);
    assert!(appsink.is_eos());
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_flush_stop_resumes_processing() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline();
    pipeline.set_state(gst::State::Playing).unwrap();

    appsrc
        .push_buffer(make_buffer(gst::ClockTime::ZERO, true))
        .unwrap();
    let first = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&first));

    let src_pad = appsrc.static_pad("src").unwrap();
    assert!(src_pad.push_event(gst::event::FlushStart::new()));
    assert!(src_pad.push_event(gst::event::FlushStop::new(true)));

    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(src_pad.push_event(gst::event::Segment::builder(&segment).build()));

    appsrc
        .push_buffer(make_buffer(gst::ClockTime::from_mseconds(100), true))
        .unwrap();
    let resumed = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&resumed));

    appsrc.end_of_stream().unwrap();
    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_oriented_metadata_renders_overlay() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline();
    pipeline.set_state(gst::State::Playing).unwrap();

    appsrc
        .push_buffer(make_oriented_buffer(gst::ClockTime::ZERO, 0.45))
        .unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    assert!(buffer_has_drawn_pixels(&out));

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}
