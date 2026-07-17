// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Tests for the `segmaskalpha` element: a synthetic segmentation mask is turned
//! into the frame's alpha channel (no ONNX model needed).

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use gst_analytics::{AnalyticsRelationMetaClassificationExt, RelTypes};
use std::sync::Once;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const STRIDE: usize = WIDTH as usize * 4;
// The mask covers this rect (frame coords).
const RECT_X: usize = 16;
const RECT_Y: usize = 16;
const RECT_W: usize = 32;
const RECT_H: usize = 32;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

fn make_mask_buffer(width: u32, height: u32, values: Vec<u8>) -> gst::Buffer {
    let mut mask = gst::Buffer::from_mut_slice(values);
    gst_video::VideoMeta::add(
        mask.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Gray8,
        width,
        height,
    )
    .unwrap();
    mask
}

fn add_segmentation_mtd(
    relation: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    mask: gst::Buffer,
    loc_x: i32,
    loc_y: i32,
    loc_w: u32,
    loc_h: u32,
) -> u32 {
    let mut region_ids = vec![0_u32, 1_u32, 2_u32, 3_u32];
    let mut seg_mtd =
        std::mem::MaybeUninit::<gst_analytics::ffi::GstAnalyticsSegmentationMtd>::uninit();

    let success: bool = unsafe {
        from_glib(
            gst_analytics::ffi::gst_analytics_relation_meta_add_segmentation_mtd(
                relation.as_mut_ptr(),
                mask.into_glib_ptr(),
                gst_analytics::ffi::GST_SEGMENTATION_TYPE_SEMANTIC,
                region_ids.len(),
                region_ids.as_mut_ptr(),
                loc_x,
                loc_y,
                loc_w,
                loc_h,
                seg_mtd.as_mut_ptr(),
            ),
        )
    };
    assert!(success, "failed to add segmentation metadata");
    unsafe { seg_mtd.assume_init() }.id
}

/// A 64×64 RGBA buffer with `fill_alpha` everywhere; optionally a segmentation
/// mask (uniform value 1 = "person") over the rect, with optional classification.
fn make_buffer(fill_alpha: u8, with_mask: bool, with_classification: bool) -> gst::Buffer {
    let mut data = vec![0u8; STRIDE * HEIGHT as usize];
    for px in data.chunks_exact_mut(4) {
        px[3] = fill_alpha;
    }
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);
        gst_video::VideoMeta::add(
            buffer_ref,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgba,
            WIDTH,
            HEIGHT,
        )
        .unwrap();

        if with_mask {
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
            let mask = make_mask_buffer(8, 8, vec![1u8; 64]);
            let seg_id = add_segmentation_mtd(
                &mut relation,
                mask,
                RECT_X as i32,
                RECT_Y as i32,
                RECT_W as u32,
                RECT_H as u32,
            );
            if with_classification {
                let classes = [
                    glib::Quark::from_str("background"),
                    glib::Quark::from_str("person"),
                    glib::Quark::from_str("car"),
                    glib::Quark::from_str("dog"),
                ];
                let levels = [1.0_f32; 4];
                let cls_id = relation.add_cls_mtd(&levels, &classes).unwrap().id();
                relation
                    .set_relation(RelTypes::N_TO_N, seg_id, cls_id)
                    .unwrap();
            }
        }
    }
    buffer
}

fn harness(props: &[(&str, &dyn ToValue)]) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("segmaskalpha");
    h.set_src_caps_str("video/x-raw,format=RGBA,width=64,height=64,framerate=1/1");
    h.set_sink_caps_str("video/x-raw,format=RGBA,width=64,height=64,framerate=1/1");
    for (name, value) in props {
        h.element()
            .unwrap()
            .set_property_from_value(name, &value.to_value());
    }
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(h.push_event(gst::event::Segment::builder(&segment).build()));
    h
}

/// Read the output alpha at frame pixel (x, y).
fn alpha_at(buffer: &gst::Buffer, x: usize, y: usize) -> u8 {
    let map = buffer.map_readable().unwrap();
    map.as_slice()[y * STRIDE + x * 4 + 3]
}

fn push_pull(h: &mut gst_check::Harness, buffer: gst::Buffer) -> gst::Buffer {
    h.push(buffer).unwrap();
    h.pull().unwrap()
}

const CENTER: (usize, usize) = (RECT_X + RECT_W / 2, RECT_Y + RECT_H / 2);
const OUTSIDE: (usize, usize) = (2, 2);

#[test]
fn writes_alpha_inside_mask_region() {
    init();
    let mut h = harness(&[]);
    let out = push_pull(&mut h, make_buffer(0, true, false));
    // Object region opaque, background transparent.
    assert_eq!(alpha_at(&out, CENTER.0, CENTER.1), 255);
    assert_eq!(alpha_at(&out, OUTSIDE.0, OUTSIDE.1), 0);
}

#[test]
fn invert_swaps_foreground_and_background() {
    init();
    let mut h = harness(&[("invert", &true)]);
    let out = push_pull(&mut h, make_buffer(0, true, false));
    assert_eq!(alpha_at(&out, CENTER.0, CENTER.1), 0);
    assert_eq!(alpha_at(&out, OUTSIDE.0, OUTSIDE.1), 255);
}

#[test]
fn no_meta_leaves_alpha_untouched() {
    init();
    let mut h = harness(&[]);
    // No segmentation meta: the incoming alpha (200) must survive.
    let out = push_pull(&mut h, make_buffer(200, false, false));
    assert_eq!(alpha_at(&out, CENTER.0, CENTER.1), 200);
    assert_eq!(alpha_at(&out, OUTSIDE.0, OUTSIDE.1), 200);
}

#[test]
fn selected_types_excludes_unselected_class() {
    init();
    // The mask is "person"; selecting only "car" leaves nothing as foreground.
    let mut h = harness(&[("selected-types", &"car")]);
    let out = push_pull(&mut h, make_buffer(0, true, true));
    assert_eq!(alpha_at(&out, CENTER.0, CENTER.1), 0);
    assert_eq!(alpha_at(&out, OUTSIDE.0, OUTSIDE.1), 0);
}

#[test]
fn selected_types_includes_matching_class() {
    init();
    let mut h = harness(&[("selected-types", &"person")]);
    let out = push_pull(&mut h, make_buffer(0, true, true));
    assert_eq!(alpha_at(&out, CENTER.0, CENTER.1), 255);
    assert_eq!(alpha_at(&out, OUTSIDE.0, OUTSIDE.1), 0);
}
