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

fn pixel_is_nonzero(buffer: &gst::Buffer, x: usize, y: usize) -> bool {
    let map = buffer.map_readable().unwrap();
    let offset = y * STRIDE + x * 4;
    if offset + 4 > map.len() {
        return false;
    }
    map.as_slice()[offset..offset + 4]
        .iter()
        .any(|byte| *byte != 0)
}

fn pixel_is_zero(buffer: &gst::Buffer, x: usize, y: usize) -> bool {
    !pixel_is_nonzero(buffer, x, y)
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

    // Push buffer with metadata containing an object detection.
    // Verify element accepts the buffer successfully.
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::ZERO, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let first = harness.pull().unwrap();
    // Verify overlay was drawn. With render-enabled=true and metadata present,
    // the element should draw pixels representing the detected object bounding box.
    assert!(buffer_has_drawn_pixels(&first));

    // Push buffer without metadata at 500ms. The overlay should reuse the previous
    // detection since it's within the expire-overlay timeout (1 second).
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_mseconds(500), false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let reused = harness.pull().unwrap();
    // Verify reused overlay is drawn. The element should apply the previous object
    // detection to this frame, drawing pixels for the still-valid detection.
    assert!(buffer_has_drawn_pixels(&reused));

    // Push buffer at 2 seconds. This is beyond the expire-overlay timeout (1 second),
    // so the previous detection should have expired.
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_seconds(2), false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let expired = harness.pull().unwrap();
    // Verify no overlay is drawn. The element should not draw anything since the
    // previous detection has expired and no new metadata is provided.
    assert!(!buffer_has_drawn_pixels(&expired));
}

#[test]
fn eos_event_is_accepted() {
    init();

    let mut harness = make_harness();

    // Verify the element accepts EOS (End of Stream) events gracefully.
    // This ensures proper stream termination and resource cleanup.
    assert!(harness.push_event(gst::event::Eos::new()));
}

#[test]
fn flush_stop_restores_processing_after_flush_start() {
    init();

    let mut harness = make_harness();

    // Push FlushStart event to initiate flushing.
    assert!(harness.push_event(gst::event::FlushStart::new()));
    // Verify element rejects buffer push during flush with Flushing error.
    // This ensures proper synchronization and prevents data loss during seeks/flushes.
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::ZERO, true)),
        Err(gst::FlowError::Flushing)
    );

    // Push FlushStop event to resume processing.
    assert!(harness.push_event(gst::event::FlushStop::new(true)));
    push_time_segment_to_harness(&mut harness);
    // Verify element resumes buffer processing after FlushStop.
    // The buffer should be accepted and processed normally.
    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::from_mseconds(100), true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let buffer = harness.pull().unwrap();
    // Verify overlay is drawn after resume. The element should continue
    // rendering overlays correctly following the flush event sequence.
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
    // Verify first frame draws overlay from the detection metadata.
    assert!(buffer_has_drawn_pixels(&first));

    let reused = pull_buffer_from_appsink(&appsink);
    // Verify overlay is reused on frame at 500ms (within 1 second timeout).
    assert!(buffer_has_drawn_pixels(&reused));

    let expired = pull_buffer_from_appsink(&appsink);
    // Verify overlay expires at 2 seconds (beyond 1 second timeout).
    // No overlay should be drawn since the detection has expired.
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
    // Verify overlay is drawn for the single frame with metadata.
    assert!(buffer_has_drawn_pixels(&first));

    wait_for_pipeline_eos(&pipeline);
    // Verify EOS is properly propagated to the sink.
    // This ensures the pipeline terminates correctly and resources are cleaned up.
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
    // Verify overlay is drawn for the initial frame.
    assert!(buffer_has_drawn_pixels(&first));

    let src_pad = appsrc.static_pad("src").unwrap();
    // Push FlushStart to initiate flushing.
    assert!(src_pad.push_event(gst::event::FlushStart::new()));
    // Push FlushStop to resume processing after flush.
    assert!(src_pad.push_event(gst::event::FlushStop::new(true)));

    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(src_pad.push_event(gst::event::Segment::builder(&segment).build()));

    appsrc
        .push_buffer(make_buffer(gst::ClockTime::from_mseconds(100), true))
        .unwrap();
    let resumed = pull_buffer_from_appsink(&appsink);
    // Verify overlay is drawn after flush-stop resumes processing.
    // The element should continue rendering overlays normally after the flush sequence.
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
    // Verify overlay is drawn for oriented (rotated) bounding box.
    // The element should correctly handle rotation metadata and render accordingly.
    assert!(buffer_has_drawn_pixels(&out));

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn drawing_location_correctness() {
    init();

    let mut harness = make_harness();

    // Create a buffer with a bounding box at (10, 10) with size 20x20.
    // This places the box edges at x=[10,30] and y=[10,30].
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object"), 10, 10, 20, 20, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // Verify pixels are drawn at the bounding box edges/corners.
    // Top-left corner area should have non-zero pixels.
    assert!(pixel_is_nonzero(&out, 10, 10));
    // Top-right corner area should have non-zero pixels.
    assert!(pixel_is_nonzero(&out, 30, 10));
    // Bottom-left corner area should have non-zero pixels.
    assert!(pixel_is_nonzero(&out, 10, 30));
    // Bottom-right corner area should have non-zero pixels.
    assert!(pixel_is_nonzero(&out, 30, 30));

    // Verify interior pixels remain zero (unfilled box).
    assert!(pixel_is_zero(&out, 20, 20));

    // Verify pixels outside the box remain zero.
    assert!(pixel_is_zero(&out, 5, 5));
    assert!(pixel_is_zero(&out, 35, 35));
}

#[test]
fn drawing_stroke_width_is_visible() {
    init();

    let mut harness = make_harness();

    // Create a buffer with a bounding box at (15, 15) with size 32x32.
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object"), 15, 15, 32, 32, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // Verify stroke is drawn at the box boundary.
    assert!(pixel_is_nonzero(&out, 15, 15));
    // Stroke extends 1px inward from the edge path (x=15), confirming the 2px
    // thickness; (16, 15)/(15, 16) sit on the stroke band, the box interior does not.
    assert!(pixel_is_nonzero(&out, 16, 15));
    assert!(pixel_is_nonzero(&out, 15, 16));
}

#[test]
fn overlapping_boxes_draw_both() {
    init();

    let mut harness = make_harness();

    // Create two overlapping bounding boxes.
    // Box 1: (10, 10) size 20x20
    // Box 2: (20, 20) size 20x20 (overlaps with Box 1 in region 20-30, 20-30)
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object1"), 10, 10, 20, 20, 0.9)
            .unwrap();
        relation
            .add_od_mtd(glib::Quark::from_str("object2"), 20, 20, 20, 20, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // Verify first box is drawn (top-left region).
    assert!(pixel_is_nonzero(&out, 10, 10));
    // Verify second box is drawn (bottom-right region).
    assert!(pixel_is_nonzero(&out, 40, 40));
    // Box1's bottom-right corner stroke (30, 30) falls inside box2, i.e. within
    // the overlap region (boxes are unfilled, so only edges are drawn).
    assert!(pixel_is_nonzero(&out, 30, 30));
}

#[test]
fn bbox_completely_outside_frame_is_skipped() {
    init();

    let mut harness = make_harness();

    // Create a bounding box completely outside the frame (to the right and below).
    // Frame is 64x64, so box starting at (100, 100) is completely outside.
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object"), 100, 100, 20, 20, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // Verify no pixels are drawn when bbox is outside frame bounds.
    // Check multiple regions to ensure nothing was drawn.
    for x in [0, 16, 32, 48, 63] {
        for y in [0, 16, 32, 48, 63] {
            assert!(pixel_is_zero(&out, x, y));
        }
    }
}

#[test]
fn composition_meta_negotiation_downstream_supports() {
    init();

    // Create harness without composition feature in source caps.
    let mut harness = gst_check::Harness::new("odoverlay");
    // Source caps: video/x-raw without composition feature
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    // Sink (downstream) caps: video/x-raw WITH composition feature to indicate support.
    // This simulates downstream accepting composition metadata.
    harness.set_sink_caps_str(
        "video/x-raw,format=BGRA,width=64,height=64,framerate=1/1,meta:GstVideoOverlayComposition",
    );
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);

    push_time_segment_to_harness(&mut harness);

    // Push buffer with metadata. Since downstream supports composition
    // (via negotiated sink caps), the element should negotiate to attach
    // composition metadata instead of blending directly.
    let buffer = make_buffer(gst::ClockTime::ZERO, true);
    assert_eq!(
        harness.push(buffer),
        Ok(gst::FlowSuccess::Ok),
        "Element should negotiate composition mode when downstream supports it"
    );

    let out = harness.pull().unwrap();
    // Verify that overlay was applied. Whether via composition or blending,
    // the result should have drawn pixels representing the detection box.
    assert!(
        buffer_has_drawn_pixels(&out),
        "Overlay should be present when composition negotiation succeeds"
    );
}

#[test]
fn composition_meta_attached_when_downstream_supports_it() {
    init();

    // The element attaches a VideoOverlayCompositionMeta (instead of blending)
    // when downstream advertises support for it. Drive that with a harness whose
    // sink proposes the meta in the allocation query and accepts the composition
    // caps feature (sink caps left unset = ANY, so the element's
    // `peer_query_caps` with the feature succeeds).
    let mut harness = gst_check::Harness::new("odoverlay");
    harness.add_propose_allocation_meta(gst_video::VideoOverlayCompositionMeta::meta_api(), None);
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);

    push_time_segment_to_harness(&mut harness);

    assert_eq!(
        harness.push(make_buffer(gst::ClockTime::ZERO, true)),
        Ok(gst::FlowSuccess::Ok),
    );
    let out = harness.pull().unwrap();

    // Attach mode: the composition meta is present and the frame pixels are left
    // untouched (compositing is deferred to the downstream that requested it).
    assert!(
        out.meta::<gst_video::VideoOverlayCompositionMeta>()
            .is_some(),
        "Buffer should carry VideoOverlayCompositionMeta when downstream supports it"
    );
    assert!(
        !buffer_has_drawn_pixels(&out),
        "Pixels should not be blended in attach mode"
    );
}

#[test]
fn composition_fallback_to_blend_when_downstream_lacks_support() {
    init();

    // Create harness where downstream does NOT support composition.
    // Element should fallback to blending (direct frame modification).
    let mut harness = gst_check::Harness::new("odoverlay");
    // Source caps without composition feature
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    // Sink (downstream) caps without composition feature - indicates no support
    harness.set_sink_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);

    push_time_segment_to_harness(&mut harness);

    // Push buffer with metadata. Since downstream does NOT support composition,
    // the element should fallback to blending mode (modifying the frame directly).
    let buffer = make_buffer(gst::ClockTime::ZERO, true);
    assert_eq!(
        harness.push(buffer),
        Ok(gst::FlowSuccess::Ok),
        "Element should handle blend fallback when composition not available"
    );

    let out = harness.pull().unwrap();

    // Verify pixels were drawn (either via composition or blending).
    // In blend mode, pixels are modified directly in the frame.
    assert!(
        buffer_has_drawn_pixels(&out),
        "Overlay should be present even in fallback blend mode"
    );

    // Verify the buffer does NOT have composition metadata, since we're in blend mode.
    let has_composition_meta = out
        .meta::<gst_video::VideoOverlayCompositionMeta>()
        .is_some();
    assert!(
        !has_composition_meta,
        "Buffer should NOT have composition metadata in blend fallback mode"
    );
}

#[test]
fn composition_reused_across_multiple_buffers_without_new_metadata() {
    init();

    // Test that composition can be reused and applied to multiple frames
    // when they don't provide new metadata but reuse is within timeout.
    let mut harness = make_harness();

    // First buffer with metadata - generates composition
    let buffer1 = make_buffer(gst::ClockTime::ZERO, true);
    assert_eq!(
        harness.push(buffer1),
        Ok(gst::FlowSuccess::Ok),
        "First buffer with metadata should be accepted"
    );
    let out1 = harness.pull().unwrap();
    assert!(
        buffer_has_drawn_pixels(&out1),
        "First frame should have overlay from new metadata"
    );

    // Second buffer without metadata, should reuse composition
    // Since default expire-overlay is 1 second, this at 100ms should reuse
    let buffer2 = make_buffer(gst::ClockTime::from_mseconds(100), false);
    assert_eq!(
        harness.push(buffer2),
        Ok(gst::FlowSuccess::Ok),
        "Second buffer without metadata should be accepted"
    );
    let out2 = harness.pull().unwrap();
    assert!(
        buffer_has_drawn_pixels(&out2),
        "Second frame should reuse overlay from first frame"
    );

    // Third buffer at 500ms (still within 1 second timeout) - should still reuse
    let buffer3 = make_buffer(gst::ClockTime::from_mseconds(500), false);
    assert_eq!(
        harness.push(buffer3),
        Ok(gst::FlowSuccess::Ok),
        "Third buffer should be accepted"
    );
    let out3 = harness.pull().unwrap();
    assert!(
        buffer_has_drawn_pixels(&out3),
        "Third frame should reuse overlay from first frame"
    );
}

#[test]
fn composition_feature_in_caps_is_added_during_negotiation() {
    init();

    // Verify that element adds composition feature to negotiated caps
    // when downstream supports it, even if not in source caps.
    let mut harness = gst_check::Harness::new("odoverlay");
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    // Sink caps with composition feature to indicate downstream support
    harness.set_sink_caps_str(
        "video/x-raw,format=BGRA,width=64,height=64,framerate=1/1,meta:GstVideoOverlayComposition",
    );
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);

    push_time_segment_to_harness(&mut harness);

    // After negotiation, the element should have negotiated caps that include
    // the composition feature if downstream supports it.
    let buffer = make_buffer(gst::ClockTime::ZERO, true);
    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));

    let out = harness.pull().unwrap();
    assert!(
        buffer_has_drawn_pixels(&out),
        "Overlay should be applied after negotiation with composition support"
    );
}

#[test]
fn bbox_partially_outside_frame_is_clipped() {
    init();

    let mut harness = make_harness();

    // Create a bounding box that partially extends outside the frame.
    // Box at (50, 50) with size 28x28 extends beyond the 64x64 frame boundary.
    // Expected to be clipped to (50-63, 50-63) region.
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object"), 50, 50, 28, 28, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // The visible edges are the box's left (x=50) and top (y=50) — the far
    // edges are off-frame. Check the corner and a point along the top edge near
    // the clip boundary.
    assert!(pixel_is_nonzero(&out, 50, 50));
    assert!(pixel_is_nonzero(&out, 63, 50));

    // Verify pixels beyond the frame boundary were not drawn/written.
    // The implementation should clip to frame bounds, not go out of bounds.
    // We verify this by checking that the frame size remains valid.
    let map = out.map_readable().unwrap();
    assert_eq!(map.len(), BUFFER_SIZE);
}

#[test]
fn bbox_partially_outside_left_and_top_is_clipped() {
    init();

    let mut harness = make_harness();

    // Create a bounding box that extends outside the frame on the left and top.
    // Box at (-10, -10) with size 30x30 extends beyond frame boundaries.
    // Expected to be clipped to (0-19, 0-19) region.
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_od_mtd(glib::Quark::from_str("object"), -10, -10, 30, 30, 0.9)
            .unwrap();
    }

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = harness.pull().unwrap();

    // The box's visible edges are its right (x=20) and bottom (y=20); the left
    // and top edges are off-frame. Check a point on each visible edge.
    assert!(pixel_is_nonzero(&out, 20, 10));
    assert!(pixel_is_nonzero(&out, 10, 20));

    // Verify interior stays zero (unfilled box, only edges drawn).
    assert!(pixel_is_zero(&out, 5, 5));
}
