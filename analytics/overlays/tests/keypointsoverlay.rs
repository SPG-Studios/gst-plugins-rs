// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_analytics::{
    AnalyticsKeypointDimensions, AnalyticsKeypointPosition, AnalyticsKeypointVisibility,
    AnalyticsRelationMetaGroupExt, AnalyticsRelationMetaKeypointExt,
};

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
    let mut harness = gst_check::Harness::new("keypointsoverlay");
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness.set_sink_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness
        .element()
        .unwrap()
        .set_property("render-enabled", true);
    harness
        .element()
        .unwrap()
        .set_property("draw-labels", false);
    push_time_segment_to_harness(&mut harness);
    harness
}

fn push_time_segment_to_harness(harness: &mut gst_check::Harness) {
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::builder(&segment).build()));
}

fn pixel_is_nonzero(buffer: &gst::Buffer, x: usize, y: usize) -> bool {
    let map = buffer.map_readable().unwrap();
    let offset = y * STRIDE + x * 4;
    map.as_slice()[offset..offset + 4]
        .iter()
        .any(|byte| *byte != 0)
}

fn pixel_is_zero(buffer: &gst::Buffer, x: usize, y: usize) -> bool {
    !pixel_is_nonzero(buffer, x, y)
}

fn make_buffer_with_single_keypoint(
    pts: gst::ClockTime,
    x: i32,
    y: i32,
    confidence: f32,
) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(pts);

        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        relation
            .add_keypoint_mtd(
                AnalyticsKeypointDimensions::_2d,
                x,
                y,
                0,
                AnalyticsKeypointVisibility::VISIBLE,
                confidence,
            )
            .unwrap();
    }

    buffer
}

fn make_buffer_with_group_and_relation(
    pts: gst::ClockTime,
    semantic_tag: &str,
    points: &[(i32, i32)],
) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(pts);

        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        let positions = points
            .iter()
            .map(|(x, y)| AnalyticsKeypointPosition {
                x: *x,
                y: *y,
                z: 0,
                dimension: AnalyticsKeypointDimensions::_2d,
            })
            .collect::<Vec<_>>();

        let group = relation
            .add_keypoints_group_from_positions(semantic_tag, &positions, None, None, &[0, 1])
            .unwrap();

        let keypoint_ids = group
            .iter::<gst_analytics::AnalyticsKeypointMtd>()
            .map(|keypoint| keypoint.id())
            .collect::<Vec<_>>();
        relation
            .set_relation(
                gst_analytics::RelTypes::RELATE_TO,
                keypoint_ids[0],
                keypoint_ids[1],
            )
            .unwrap();
    }

    buffer
}

#[test]
fn renders_individual_keypoint() {
    init();

    let mut harness = make_harness();

    // Push buffer with metadata containing a single keypoint at (24, 24).
    // Verify element accepts the buffer successfully.
    assert_eq!(
        harness.push(make_buffer_with_single_keypoint(
            gst::ClockTime::ZERO,
            24,
            24,
            0.9
        )),
        Ok(gst::FlowSuccess::Ok)
    );

    let buffer = harness.pull().unwrap();
    // Verify pixel at keypoint location (24, 24) is drawn with non-zero color.
    assert!(pixel_is_nonzero(&buffer, 24, 24));
    // Verify unrelated pixel at (0, 0) remains zero (no spurious drawing).
    assert!(pixel_is_zero(&buffer, 0, 0));
}

#[test]
fn renders_group_skeleton_when_semantic_tag_matches() {
    init();

    let mut harness = make_harness();
    // Enable skeleton rendering (lines connecting keypoints).
    harness
        .element()
        .unwrap()
        .set_property("draw-skeleton", true);
    // Filter to only render groups with semantic tag starting with "pose/".
    harness
        .element()
        .unwrap()
        .set_property("semantic-tag", Some("pose/".to_string()));

    // Push buffer with keypoint group tagged "pose/body" containing two keypoints at (12, 20) and (52, 20).
    assert_eq!(
        harness.push(make_buffer_with_group_and_relation(
            gst::ClockTime::ZERO,
            "pose/body",
            &[(12, 20), (52, 20)],
        )),
        Ok(gst::FlowSuccess::Ok)
    );

    let buffer = harness.pull().unwrap();
    // Verify first keypoint is drawn at (12, 20).
    assert!(pixel_is_nonzero(&buffer, 12, 20));
    // Verify second keypoint is drawn at (52, 20).
    assert!(pixel_is_nonzero(&buffer, 52, 20));
    // Verify skeleton line between keypoints is drawn at midpoint (32, 20).
    // This tests that the element connects related keypoints correctly.
    assert!(pixel_is_nonzero(&buffer, 32, 20));
}

#[test]
fn skips_keypoints_outside_frame() {
    init();

    let mut harness = make_harness();

    // Push buffer with keypoint at x=-6 (outside frame bounds to the left).
    // Verify element accepts the buffer.
    assert_eq!(
        harness.push(make_buffer_with_single_keypoint(
            gst::ClockTime::ZERO,
            -6,
            24,
            0.9
        )),
        Ok(gst::FlowSuccess::Ok)
    );

    let buffer = harness.pull().unwrap();
    // Verify no pixels are drawn at (0, 0). Out-of-bounds keypoints should not
    // cause any drawing at frame edges, preventing clipping artifacts.
    assert!(pixel_is_zero(&buffer, 0, 0));
    // Verify unrelated pixel remains zero (sanity check).
    assert!(pixel_is_zero(&buffer, 24, 24));
}
